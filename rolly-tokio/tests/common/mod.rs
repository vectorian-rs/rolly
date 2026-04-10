//! Shared test helpers for rolly-tokio integration tests.

use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Spawn a TCP server that accepts HTTP requests, stores bodies, and responds
/// with the given status line.
pub fn spawn_http_server(
    listener: TcpListener,
    status: &'static str,
    bodies: Arc<tokio::sync::Mutex<Vec<Vec<u8>>>>,
) {
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let bodies = bodies.clone();
            tokio::spawn(async move {
                if let Some(body) = read_http_body(&mut stream).await {
                    bodies.lock().await.push(body);
                }
                let resp = format!("HTTP/1.1 {}\r\nContent-Length: 0\r\n\r\n", status);
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });
}

/// Read a full HTTP request from a stream and return the body bytes.
pub async fn read_http_body(stream: &mut tokio::net::TcpStream) -> Option<Vec<u8>> {
    let mut buf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 4096];

    // Read until headers are complete
    loop {
        let n = stream.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }

    let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let header_str = String::from_utf8_lossy(&buf[..header_end]);
    let content_length: usize = header_str
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);

    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }

    Some(body)
}

/// Create a layer + exporter pointed at the given address.
pub fn make_layer_and_exporter(
    addr: std::net::SocketAddr,
) -> (
    impl tracing_subscriber::Layer<tracing_subscriber::Registry>,
    rolly_tokio::TokioExporter,
) {
    use rolly_tokio::{BackpressureStrategy, ExporterConfig, LayerConfig, TelemetrySink};

    let exporter = rolly_tokio::TokioExporter::start(ExporterConfig {
        traces_url: Some(format!("http://{}/v1/traces", addr)),
        logs_url: None,
        metrics_url: None,
        flush_interval: std::time::Duration::from_millis(10),
        backpressure_strategy: BackpressureStrategy::Drop,
        ..ExporterConfig::default()
    });

    let sink: Arc<dyn TelemetrySink> = Arc::new(exporter.clone());
    let layer_config = LayerConfig {
        log_to_stderr: false,
        export_traces: true,
        export_logs: false,
        service_name: "test-svc".into(),
        service_version: "0.0.1".into(),
        environment: "test".into(),
        resource_attributes: vec![],
        sampling_rate: 1.0,
    };
    let layer = rolly::build_layer(&layer_config, sink);
    (layer, exporter)
}
