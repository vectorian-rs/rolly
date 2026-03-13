#![cfg(feature = "_bench")]

//! Backpressure tests: prove the hot path never blocks when the export channel is full.

use std::sync::{Arc, Barrier};
use std::time::Instant;

use rolly::bench::{Exporter, OtlpLayer};
use tracing_subscriber::layer::SubscriberExt;

/// Create a Dispatch backed by an OtlpLayer with a test exporter of the given capacity.
/// Returns the dispatch and the receiver (hold it to keep the channel open).
fn make_dispatch(
    capacity: usize,
    export_traces: bool,
    export_logs: bool,
    sampling_rate: f64,
) -> (
    tracing::Dispatch,
    tokio::sync::mpsc::Receiver<rolly::bench::ExportMessage>,
) {
    let (exporter, rx) = Exporter::start_test_with_capacity(capacity);
    let layer = OtlpLayer::new(
        exporter,
        "bp-test",
        "0.0.1",
        "test",
        export_traces,
        export_logs,
        sampling_rate,
    );
    let subscriber = tracing_subscriber::registry().with(layer);
    (tracing::Dispatch::new(subscriber), rx)
}

#[test]
fn hot_path_does_not_block_when_channel_full() {
    let before = rolly::telemetry_dropped_total();
    let (dispatch, _rx) = make_dispatch(4, true, false, 1.0);

    let start = Instant::now();
    tracing::dispatcher::with_default(&dispatch, || {
        for _ in 0..1000 {
            let span = tracing::info_span!("bp-span");
            let _enter = span.enter();
        }
    });
    let elapsed = start.elapsed();

    let delta = rolly::telemetry_dropped_total() - before;
    assert!(
        elapsed.as_millis() < 500,
        "hot path blocked: took {}ms (limit 500ms)",
        elapsed.as_millis()
    );
    assert!(
        delta >= 996,
        "expected at least 996 drops (1000 spans - 4 capacity), got {}",
        delta
    );
}

#[test]
fn hot_path_nonblocking_under_concurrent_pressure() {
    let before = rolly::telemetry_dropped_total();
    let (dispatch, _rx) = make_dispatch(4, true, false, 1.0);
    let barrier = Arc::new(Barrier::new(8));

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let dispatch = dispatch.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let start = Instant::now();
                tracing::dispatcher::with_default(&dispatch, || {
                    for _ in 0..500 {
                        let span = tracing::info_span!("pressure-span");
                        let _enter = span.enter();
                    }
                });
                start.elapsed()
            })
        })
        .collect();

    let max_elapsed = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .max()
        .unwrap();

    let delta = rolly::telemetry_dropped_total() - before;
    assert!(
        max_elapsed.as_secs() < 2,
        "slowest thread took {:?} (limit 2s)",
        max_elapsed
    );
    // 8 threads x 500 spans = 4000 total, minus up to 4 that fit in channel
    assert!(delta >= 3996, "expected at least 3996 drops, got {}", delta);
}

#[test]
fn dropped_total_increments_accurately() {
    let (dispatch, _rx) = make_dispatch(1, true, false, 1.0);

    // Fill the single-capacity channel with one span
    tracing::dispatcher::with_default(&dispatch, || {
        let span = tracing::info_span!("fill-span");
        let _enter = span.enter();
    });

    // Channel is now full -- next 100 sends should all be dropped
    let before = rolly::telemetry_dropped_total();
    tracing::dispatcher::with_default(&dispatch, || {
        for _ in 0..100 {
            let span = tracing::info_span!("drop-span");
            let _enter = span.enter();
        }
    });

    let delta = rolly::telemetry_dropped_total() - before;
    // Use >= because the global AtomicU64 may be incremented by parallel tests
    assert!(delta >= 100, "expected at least 100 drops, got {}", delta);
}

#[test]
fn events_also_nonblocking_under_backpressure() {
    let before = rolly::telemetry_dropped_total();
    let (dispatch, _rx) = make_dispatch(2, false, true, 1.0);

    let start = Instant::now();
    tracing::dispatcher::with_default(&dispatch, || {
        for _ in 0..1000 {
            tracing::info!("backpressure-event");
        }
    });
    let elapsed = start.elapsed();

    let delta = rolly::telemetry_dropped_total() - before;
    assert!(
        elapsed.as_millis() < 500,
        "event hot path blocked: took {}ms (limit 500ms)",
        elapsed.as_millis()
    );
    assert!(
        delta >= 998,
        "expected at least 998 drops (1000 events - 2 capacity), got {}",
        delta
    );
}
