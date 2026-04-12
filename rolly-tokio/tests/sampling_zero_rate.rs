#![cfg(feature = "_bench")]

//! Verifies that sampled-out spans produce zero channel messages,
//! even under high concurrency. Moved from rolly core since it
//! depends on the tokio-based Exporter.

use std::sync::{Arc, Barrier};

use rolly_tokio::bench::{Exporter, OtlpLayer, OtlpLayerConfig};
use tracing_subscriber::layer::SubscriberExt;

#[test]
fn sampled_out_produces_zero_channel_messages_under_concurrency() {
    let before = rolly_tokio::telemetry_dropped_total();
    let (exporter, mut rx) =
        Exporter::start_test_with_capacity(1024, rolly_tokio::bench::BackpressureStrategy::Drop);
    let layer = OtlpLayer::new(OtlpLayerConfig {
        sink: std::sync::Arc::new(exporter),
        service_name: "sample-test",
        service_version: "0.0.1",
        environment: "test",
        resource_attributes: &[],
        export_traces: true,
        export_logs: true,
        sampling_rate: 0.0,
        scope_name: "rolly",
        scope_version: "test",
    });
    let subscriber = tracing_subscriber::registry().with(layer);
    let dispatch = tracing::Dispatch::new(subscriber);

    let barrier = Arc::new(Barrier::new(8));

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let dispatch = dispatch.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                tracing::dispatcher::with_default(&dispatch, || {
                    for _ in 0..500 {
                        let span = tracing::info_span!("sampled-out");
                        let _enter = span.enter();
                        tracing::info!("sampled-out-event");
                    }
                });
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    // No messages should have been sent (sampled-out spans and their events are suppressed)
    assert!(
        rx.try_recv().is_err(),
        "expected no messages in channel when sampling_rate=0.0"
    );

    // No drops either -- sends were never attempted
    let delta = rolly_tokio::telemetry_dropped_total() - before;
    assert_eq!(
        delta, 0,
        "expected 0 drops (sends never attempted), got {}",
        delta
    );
}
