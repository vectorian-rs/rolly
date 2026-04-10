//! Shutdown and flush reliability tests.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tracing_subscriber::layer::SubscriberExt;

#[tokio::test(flavor = "multi_thread")]
async fn all_spans_arrive_after_flush_and_shutdown() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let bodies = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    common::spawn_http_server(listener, "200 OK", bodies.clone());

    let (layer, exporter) = common::make_layer_and_exporter(addr);
    let subscriber = tracing_subscriber::registry().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);

    for _ in 0..50 {
        let span = tracing::info_span!("flush-span");
        let _enter = span.enter();
    }

    exporter.flush().await;
    exporter.shutdown().await;

    let received = bodies.lock().await;
    let all_bytes: Vec<u8> = received.iter().flatten().copied().collect();
    let count = all_bytes
        .windows(10)
        .filter(|w| *w == b"flush-span")
        .count();
    assert!(
        count >= 50,
        "expected at least 50 'flush-span' in HTTP bodies, got {}",
        count
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_completes_when_endpoint_is_failing() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let bodies = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    common::spawn_http_server(listener, "500 Internal Server Error", bodies);

    let (layer, exporter) = common::make_layer_and_exporter(addr);
    let subscriber = tracing_subscriber::registry().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);

    {
        let span = tracing::info_span!("fail-span");
        let _enter = span.enter();
    }

    let result = tokio::time::timeout(Duration::from_secs(30), async {
        exporter.flush().await;
        exporter.shutdown().await;
    })
    .await;
    assert!(
        result.is_ok(),
        "shutdown should complete even when endpoint returns 500"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn flush_waits_for_in_flight_exports() {
    static SLOW_REQUESTS: AtomicUsize = AtomicUsize::new(0);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                // Read the request
                let _ = common::read_http_body(&mut stream).await;
                // Simulate slow backend
                tokio::time::sleep(Duration::from_millis(100)).await;
                SLOW_REQUESTS.fetch_add(1, Ordering::Relaxed);
                let resp = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    let (layer, exporter) = common::make_layer_and_exporter(addr);
    let subscriber = tracing_subscriber::registry().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);

    for _ in 0..5 {
        let span = tracing::info_span!("slow-span");
        let _enter = span.enter();
    }

    exporter.flush().await;
    exporter.shutdown().await;

    assert!(
        SLOW_REQUESTS.load(Ordering::Relaxed) > 0,
        "flush should have waited for at least one slow request to complete"
    );
}
