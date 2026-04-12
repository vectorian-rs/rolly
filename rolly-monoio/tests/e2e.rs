use std::time::Duration;

use monoio::io::{AsyncReadRent, AsyncWriteRentExt};
use monoio::net::TcpListener;

/// Accept one HTTP request, extract the body, send 200 OK.
///
/// Reads in a loop until the full request (headers + Content-Length body)
/// has been received, so it works even when data arrives in multiple reads.
async fn handle_http_request(mut stream: monoio::net::TcpStream) -> Vec<u8> {
    let mut received = Vec::new();

    // Read until we have headers + full body
    loop {
        let buf = vec![0u8; 65536];
        let (result, buf) = stream.read(buf).await;
        let n = match result {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        received.extend_from_slice(&buf[..n]);

        // Check if we have the full request
        if let Some(header_end) = received.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&received[..header_end]).unwrap_or("");
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let lower = line.to_lowercase();
                    lower
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            let body_start = header_end + 4;
            if received.len() >= body_start + content_length {
                let body = received[body_start..body_start + content_length].to_vec();
                let resp =
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
                let (result, _) = stream.write_all(resp).await;
                let _ = result;
                return body;
            }
        }
    }

    Vec::new()
}

/// Read an HTTP request from a stream and extract the URL path from the first line.
async fn extract_request_path(mut stream: monoio::net::TcpStream) -> String {
    let buf = vec![0u8; 4096];
    let (result, buf) = stream.read(buf).await;
    let n = result.unwrap_or(0);
    let request = String::from_utf8_lossy(&buf[..n]);

    let path = request
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .unwrap_or("")
        .to_string();

    let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
    let (result, _) = stream.write_all(resp).await;
    let _ = result;

    path
}

#[monoio::test(enable_timer = true)]
async fn exporter_queues_and_flushes_without_panic() {
    let config = rolly_monoio::ExporterConfig {
        traces_url: Some("http://127.0.0.1:1/v1/traces".to_string()),
        logs_url: Some("http://127.0.0.1:1/v1/logs".to_string()),
        metrics_url: None,
        channel_capacity: 16,
        batch_size: 512,
        flush_interval: Duration::from_secs(60),
        max_concurrent_exports: 4,
        max_pending_batches: 32,
        backpressure_strategy: rolly::BackpressureStrategy::Drop,
    };
    let exporter = rolly_monoio::MonoioExporter::start(config);

    exporter.send_traces(vec![0x0A, 0x00]);
    exporter.send_logs(vec![0x0A, 0x00]);

    exporter.shutdown().await;
}

#[monoio::test(enable_timer = true)]
async fn exporter_flush_completes() {
    let config = rolly_monoio::ExporterConfig {
        traces_url: Some("http://127.0.0.1:1/v1/traces".to_string()),
        logs_url: Some("http://127.0.0.1:1/v1/logs".to_string()),
        metrics_url: None,
        channel_capacity: 16,
        batch_size: 512,
        flush_interval: Duration::from_secs(60),
        max_concurrent_exports: 4,
        max_pending_batches: 32,
        backpressure_strategy: rolly::BackpressureStrategy::Drop,
    };
    let exporter = rolly_monoio::MonoioExporter::start(config);

    exporter.flush().await;
    exporter.shutdown().await;
}

#[monoio::test(enable_timer = true)]
async fn exporter_sends_traces_to_server() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let (body_tx, body_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    monoio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let body_tx = body_tx.clone();
            monoio::spawn(async move {
                let body = handle_http_request(stream).await;
                if !body.is_empty() {
                    let _ = body_tx.send(body);
                }
            });
        }
    });

    monoio::time::sleep(Duration::from_millis(50)).await;

    let config = rolly_monoio::ExporterConfig {
        traces_url: Some(format!("http://{}/v1/traces", addr)),
        logs_url: None,
        metrics_url: None,
        channel_capacity: 16,
        batch_size: 512,
        flush_interval: Duration::from_secs(60),
        max_concurrent_exports: 4,
        max_pending_batches: 32,
        backpressure_strategy: rolly::BackpressureStrategy::Drop,
    };
    let exporter = rolly_monoio::MonoioExporter::start(config);

    let payload = vec![0x0A, 0x02, 0x08, 0x01];
    exporter.send_traces(payload.clone());
    exporter.flush().await;

    let received = body_rx.recv_timeout(Duration::from_secs(5));
    assert!(received.is_ok(), "should have received HTTP body");
    assert_eq!(received.unwrap(), payload);

    exporter.shutdown().await;
}

#[monoio::test(enable_timer = true)]
async fn flush_completes_when_data_channel_is_full() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let (body_tx, body_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    monoio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let body_tx = body_tx.clone();
            monoio::spawn(async move {
                let body = handle_http_request(stream).await;
                if !body.is_empty() {
                    let _ = body_tx.send(body);
                }
            });
        }
    });

    monoio::time::sleep(Duration::from_millis(50)).await;

    let config = rolly_monoio::ExporterConfig {
        traces_url: Some(format!("http://{}/v1/traces", addr)),
        logs_url: None,
        metrics_url: None,
        channel_capacity: 1,
        batch_size: 512,
        flush_interval: Duration::from_secs(60),
        max_concurrent_exports: 4,
        max_pending_batches: 32,
        backpressure_strategy: rolly::BackpressureStrategy::Drop,
    };
    let exporter = rolly_monoio::MonoioExporter::start(config);

    let payload = vec![0x0A, 0x02, 0x08, 0x01];
    exporter.send_traces(payload.clone());
    exporter.flush().await;

    let received = body_rx.recv_timeout(Duration::from_secs(5));
    assert!(
        received.is_ok(),
        "flush should drain even when the data queue is full"
    );
    assert_eq!(received.unwrap(), payload);

    exporter.shutdown().await;
}

#[monoio::test(enable_timer = true)]
async fn exporter_batches_multiple_messages() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let (body_tx, body_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    monoio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let body_tx = body_tx.clone();
            monoio::spawn(async move {
                let body = handle_http_request(stream).await;
                if !body.is_empty() {
                    let _ = body_tx.send(body);
                }
            });
        }
    });

    monoio::time::sleep(Duration::from_millis(50)).await;

    let config = rolly_monoio::ExporterConfig {
        traces_url: Some(format!("http://{}/v1/traces", addr)),
        logs_url: None,
        metrics_url: None,
        channel_capacity: 16,
        batch_size: 3,
        flush_interval: Duration::from_secs(60),
        max_concurrent_exports: 4,
        max_pending_batches: 32,
        backpressure_strategy: rolly::BackpressureStrategy::Drop,
    };
    let exporter = rolly_monoio::MonoioExporter::start(config);

    let payload = vec![0x0A, 0x00];
    exporter.send_traces(payload.clone());
    exporter.send_traces(payload.clone());
    exporter.send_traces(payload.clone());
    exporter.flush().await;

    let received = body_rx.recv_timeout(Duration::from_secs(5));
    assert!(received.is_ok(), "should have received batched body");
    assert_eq!(received.unwrap().len(), 6);

    exporter.shutdown().await;
}

#[monoio::test(enable_timer = true)]
async fn exporter_handles_traces_and_logs() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let (path_tx, path_rx) = std::sync::mpsc::channel::<String>();

    monoio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let path_tx = path_tx.clone();
            monoio::spawn(async move {
                let path = extract_request_path(stream).await;
                let _ = path_tx.send(path);
            });
        }
    });

    monoio::time::sleep(Duration::from_millis(50)).await;

    let config = rolly_monoio::ExporterConfig {
        traces_url: Some(format!("http://{}/v1/traces", addr)),
        logs_url: Some(format!("http://{}/v1/logs", addr)),
        metrics_url: None,
        channel_capacity: 16,
        batch_size: 512,
        flush_interval: Duration::from_secs(60),
        max_concurrent_exports: 4,
        max_pending_batches: 32,
        backpressure_strategy: rolly::BackpressureStrategy::Drop,
    };
    let exporter = rolly_monoio::MonoioExporter::start(config);

    exporter.send_traces(vec![0x0A, 0x00]);
    exporter.send_logs(vec![0x0A, 0x00]);
    exporter.flush().await;

    let mut paths = Vec::new();
    while let Ok(path) = path_rx.recv_timeout(Duration::from_secs(5)) {
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

#[monoio::test(enable_timer = true)]
async fn exporter_skips_logs_when_no_logs_url() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let (path_tx, path_rx) = std::sync::mpsc::channel::<String>();

    monoio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let path_tx = path_tx.clone();
            monoio::spawn(async move {
                let path = extract_request_path(stream).await;
                let _ = path_tx.send(path);
            });
        }
    });

    monoio::time::sleep(Duration::from_millis(50)).await;

    let config = rolly_monoio::ExporterConfig {
        traces_url: Some(format!("http://{}/v1/traces", addr)),
        logs_url: None,
        metrics_url: None,
        channel_capacity: 16,
        batch_size: 512,
        flush_interval: Duration::from_secs(60),
        max_concurrent_exports: 4,
        max_pending_batches: 32,
        backpressure_strategy: rolly::BackpressureStrategy::Drop,
    };
    let exporter = rolly_monoio::MonoioExporter::start(config);

    exporter.send_traces(vec![0x0A, 0x00]);
    exporter.send_logs(vec![0x0A, 0x00]); // should be silently dropped
    exporter.flush().await;

    let mut paths = Vec::new();
    while let Ok(path) = path_rx.recv_timeout(Duration::from_millis(500)) {
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
