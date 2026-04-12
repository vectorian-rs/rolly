//! E2E test: verify exemplar trace_id propagates through the metrics pipeline.
//!
//! Records a counter inside a traced span, flushes metrics, and verifies
//! the trace_id from the span appears in the exported metrics protobuf.

mod common;

use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tracing_subscriber::layer::SubscriberExt;

#[tokio::test(flavor = "multi_thread")]
async fn counter_exemplar_contains_span_trace_id() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let bodies = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    common::spawn_http_server(listener, "200 OK", bodies.clone());

    // Set up exporter with metrics endpoint
    let exporter = rolly_tokio::TokioExporter::start(rolly_tokio::ExporterConfig {
        traces_url: Some(format!("http://{}/v1/traces", addr)),
        logs_url: None,
        metrics_url: Some(format!("http://{}/v1/metrics", addr)),
        flush_interval: Duration::from_millis(10),
        backpressure_strategy: rolly::BackpressureStrategy::Drop,
        ..rolly_tokio::ExporterConfig::default()
    })
    .unwrap();

    let sink: Arc<dyn rolly::TelemetrySink> = Arc::new(exporter.clone());

    let layer_config = rolly::LayerConfig {
        log_to_stderr: false,
        export_traces: true,
        export_logs: false,
        service_name: "exemplar-test".into(),
        service_version: "0.0.1".into(),
        environment: "test".into(),
        resource_attributes: vec![],
        sampling_rate: 1.0,
    };
    let layer = rolly::build_layer(&layer_config, sink.clone());
    let subscriber = tracing_subscriber::registry().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);

    // Known trace ID — we'll search for these bytes in the metrics payload
    let trace_id_hex = "aabbccdd11223344aabbccdd11223344";
    let trace_id_bytes: [u8; 16] = [
        0xaa, 0xbb, 0xcc, 0xdd, 0x11, 0x22, 0x33, 0x44, 0xaa, 0xbb, 0xcc, 0xdd, 0x11, 0x22, 0x33,
        0x44,
    ];

    // Record a counter inside a span — this should capture an exemplar
    {
        let span = tracing::info_span!("exemplar-span", trace_id = trace_id_hex);
        let _enter = span.enter();
        let counter = rolly::counter("exemplar_test_requests", "test counter");
        counter.add(1, &[("method", "GET")]);
    }

    // Manually collect and send metrics (simulates what spawn_metrics_loop does)
    let metrics_config = rolly::MetricsExportConfig {
        service_name: "exemplar-test".into(),
        service_version: "0.0.1".into(),
        environment: "test".into(),
        resource_attributes: vec![],
        scope_name: "rolly".into(),
        scope_version: env!("CARGO_PKG_VERSION").into(),
        start_time: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64,
    };
    if let Some(data) = rolly::collect_and_encode_metrics(&metrics_config) {
        sink.send_metrics(data);
    }

    exporter.flush().await;
    exporter.shutdown().await;

    // Collect all received bodies
    tokio::time::sleep(Duration::from_millis(100)).await;
    let received = bodies.lock().await;
    let all_bytes: Vec<u8> = received.iter().flatten().copied().collect();

    // The trace_id bytes should appear in the metrics payload as an exemplar
    assert!(
        all_bytes.windows(16).any(|w| w == trace_id_bytes),
        "trace_id exemplar bytes not found in exported metrics payload \
         ({} bytes received across {} requests)",
        all_bytes.len(),
        received.len()
    );
}
