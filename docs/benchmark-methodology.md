# Realistic Benchmark Methodology with Baseline & SVG Visualization

## Goal

Establish a repeatable performance measurement framework for a Rust library or service that:
1. Simulates realistic workloads (not micro-benchmarks of isolated functions)
2. Tracks results in a committed, human-readable TOML baseline
3. Renders a self-contained SVG visualization for README / PR review
4. Measures CPU time, memory allocations, and throughput under realistic attribute/dimension cardinality

## Architecture

```
benches/realistic_scenario.rs   ← Criterion benchmarks simulating real traffic
benches/baseline.toml           ← Committed measured results (diffable in PRs)
tools/render_baseline.rs        ← Reads TOML → renders SVG via plotters
docs/illustration/performance.svg ← Self-contained 4-panel visualization
```

## Step 1: Define the Workload Scenario

Pick a realistic usage pattern for your library. Define:

- **Operations**: 4-8 distinct operation types that represent real usage (not just `encode()` or `parse()` in isolation)
- **Traffic weights**: What percentage of total traffic each operation represents
- **Attribute cardinality**: Real-world calls have 4-6 dimensions/parameters, not 1-2. This matters because sorting, hashing, and HashMap lookups scale with dimension count.
- **Resource budget**: Define a target (e.g., "<10% of one CPU core at 3000 ops/sec" or "<50MB RSS at steady state")

Example scenario table:

```
| Operation        | Weight | Description                              |
|------------------|--------|------------------------------------------|
| op_read_simple   | 40%    | Simple read, 4 params, small response    |
| op_search        | 10%    | Search with filters, 6 params, large     |
| op_read_complex  | 20%    | Complex read with joins, 5 params        |
| op_write         | 18%    | Write with validation, 5 params          |
| op_transaction   | 8%     | Multi-step transaction, 3 child ops      |
| op_health        | 4%     | Trivial health check                     |
```

## Step 2: Baseline TOML Format

```toml
[metadata]
version = "0.1.0"
date = "2026-03-11"
platform = "arm64-darwin"    # or "x86_64-linux"
cpu = "Apple M-series"       # or "AMD EPYC 7763"
rust_version = "1.85.0"
notes = "Measured on AC power, no background load"

[benchmarks.op_read_simple]
description = "Simple read: 4 params, small response"
mean_ns = 0           # filled after benchmark run
median_ns = 0
peak_rss_bytes = 0    # peak resident set size during operation
alloc_bytes = 0       # total bytes allocated per operation (via GlobalAlloc tracker)
alloc_count = 0       # number of allocations per operation

[benchmarks.op_search]
description = "Search with filters: 6 params, large response"
mean_ns = 0
median_ns = 0
peak_rss_bytes = 0
alloc_bytes = 0
alloc_count = 0

# ... one section per operation

[traffic_weights]
op_read_simple = 0.40
op_search = 0.10
op_read_complex = 0.20
op_write = 0.18
op_transaction = 0.08
op_health = 0.04

[resource_budget]
target_ops_per_sec = 3000
cpu_budget_percent = 10          # max % of one core
cpu_budget_us_per_op = 33.3      # = 1e6 * (budget_percent/100) / target_ops_per_sec
memory_budget_mb = 50            # max steady-state RSS
weighted_avg_us = 0.0            # computed: sum(mean_us * weight)
total_cpu_percent = 0.0          # computed: weighted_avg_us * target_rps / 1e6 * 100
steady_state_rss_mb = 0.0        # measured at sustained throughput
verdict = "PENDING"              # PASS or FAIL
```

Key design decisions:
- **TOML, not JSON**: Human-readable, diffable in PRs, supports comments
- **Both mean and median**: Mean shows average cost; median resists outliers
- **Memory tracked per-operation AND at steady state**: Per-op shows allocation pressure; steady-state shows whether memory grows unbounded
- **Values start at 0**: Fill after first run. This makes the file self-documenting about what needs measuring.

## Step 3: Benchmark File Structure

```rust
// benches/realistic_scenario.rs

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use std::time::Duration;

// --- Allocation tracking ---

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, AtomicBool, Ordering};

static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static TRACKING: AtomicBool = AtomicBool::new(false);

struct TrackingAllocator;

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if TRACKING.load(Ordering::Relaxed) {
            ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: TrackingAllocator = TrackingAllocator;

fn reset_alloc_tracking() {
    ALLOC_BYTES.store(0, Ordering::Relaxed);
    ALLOC_COUNT.store(0, Ordering::Relaxed);
    TRACKING.store(true, Ordering::Relaxed);
}

fn stop_alloc_tracking() -> (u64, u64) {
    TRACKING.store(false, Ordering::Relaxed);
    (
        ALLOC_BYTES.load(Ordering::Relaxed),
        ALLOC_COUNT.load(Ordering::Relaxed),
    )
}

// --- Setup ---

struct WorkloadContext {
    // Your library's state: connection pools, caches, registries, etc.
    // Created once per benchmark group to amortize setup cost.
}

impl WorkloadContext {
    fn new() -> Self {
        // Initialize with realistic configuration
        Self { /* ... */ }
    }
}

// --- Operation simulations ---
//
// Each function simulates a COMPLETE operation with realistic parameters.
// Use #[inline(never)] to prevent the compiler from optimizing across
// operation boundaries — we want to measure the actual call overhead.

#[inline(never)]
fn simulate_read_simple(ctx: &WorkloadContext) {
    // 1. Create the operation context (4 params, like a real API call)
    // 2. Execute the core logic
    // 3. Record metrics/counters with 4-6 dimensions
    // 4. Cleanup
    todo!()
}

#[inline(never)]
fn simulate_transaction(ctx: &WorkloadContext) {
    // Multi-step: parent operation + 2-3 child operations
    // This is typically the most expensive path
    todo!()
}

#[inline(never)]
fn simulate_health() {
    // Minimal operation — establishes the baseline floor
    todo!()
}

/// Mixed batch at weighted distribution
#[inline(never)]
fn simulate_mixed_batch(ctx: &WorkloadContext, n: usize) {
    for i in 0..n {
        let pct = i % 100;
        match pct {
            0..40   => simulate_read_simple(ctx),
            // ... weighted distribution matching traffic_weights
            _       => simulate_health(),
        }
    }
}

// --- Benchmark groups ---

fn bench_read_simple(c: &mut Criterion) {
    let mut group = c.benchmark_group("op_read_simple");
    let ctx = WorkloadContext::new();

    group.bench_function("full_operation", |b| {
        b.iter(|| simulate_read_simple(black_box(&ctx)));
    });
    group.finish();
}

fn bench_mixed_traffic(c: &mut Criterion) {
    let mut group = c.benchmark_group("mixed_traffic");
    group.throughput(Throughput::Elements(100));
    let ctx = WorkloadContext::new();

    group.bench_function("100_ops", |b| {
        b.iter(|| simulate_mixed_batch(black_box(&ctx), 100));
    });
    group.finish();
}

fn bench_sustained_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("sustained_throughput");
    group.throughput(Throughput::Elements(3000));
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(15));
    let ctx = WorkloadContext::new();

    group.bench_function("3000_ops", |b| {
        b.iter(|| simulate_mixed_batch(black_box(&ctx), 3000));
    });
    group.finish();
}

// --- Memory profiling (separate from timing) ---

fn bench_memory_per_operation(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory_per_operation");
    let ctx = WorkloadContext::new();

    // Warm up to reach steady state
    for _ in 0..1000 {
        simulate_mixed_batch(&ctx, 100);
    }

    group.bench_function("read_simple_allocs", |b| {
        b.iter(|| {
            reset_alloc_tracking();
            simulate_read_simple(black_box(&ctx));
            let (bytes, count) = stop_alloc_tracking();
            black_box((bytes, count));
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_read_simple,
    // ... one per operation
    bench_mixed_traffic,
    bench_sustained_throughput,
    bench_memory_per_operation,
);
criterion_main!(benches);
```

## Step 4: SVG Renderer

Register as `[[example]]` in Cargo.toml (not `[[bin]]` — it doesn't ship):

```toml
[dev-dependencies]
plotters = { version = "0.3", default-features = false, features = ["svg_backend", "line_series"] }
toml = "0.8"
serde = { version = "1", features = ["derive"] }

[[example]]
name = "render_baseline"
```

The renderer (`tools/render_baseline.rs`) reads `benches/baseline.toml` and produces `docs/illustration/performance.svg` with panels:

1. **Per-Operation Latency** — Horizontal bar chart, one bar per operation. Color-coded: green (<70% of budget), yellow (70-100%), red (>budget). Vertical dashed line at budget threshold.

2. **Throughput Capacity** — Vertical bars showing theoretical max ops/sec per operation (1e9 / mean_ns). Horizontal red line at target ops/sec.

3. **Resource Budget** — Stacked horizontal bar showing what % of one core each operation consumes at its weighted traffic share. Budget line marked. Second row for memory: stacked bar of per-operation allocation bytes * weight * target_rps.

4. **Summary** — Text panel with weighted average, total CPU %, steady-state RSS, per-operation breakdown table, and PASS/FAIL verdict.

## Step 5: Workflow

```bash
# 1. Run benchmarks
cargo bench --features _bench -- op_

# 2. Extract results from target/criterion/*/new/estimates.json
#    Update benches/baseline.toml with mean_ns, median_ns

# 3. Run memory profiling pass (optional, separate from timing)
#    Update alloc_bytes, alloc_count, peak_rss_bytes

# 4. Compute derived fields:
#    weighted_avg_us = sum(mean_ns/1000 * weight)
#    total_cpu_percent = weighted_avg_us * target_rps / 1e6 * 100
#    verdict = "PASS" if cpu < budget AND rss < memory_budget

# 5. Regenerate SVG
cargo run --example render_baseline

# 6. Commit baseline.toml + performance.svg together
#    PR reviewers see the diff in both raw numbers and visualization
```

## Key Principles

**Realistic, not synthetic.** Each benchmark simulates a complete operation with realistic parameter counts. A function that takes 2 string args in a micro-benchmark may take 6 typed params with validation in production. The difference is 2-3x.

**Attribute cardinality matters.** If your library does any sorting, hashing, or map lookups keyed on user-provided dimensions, benchmark with 4-6 dimensions, not 1-2. Going from 2 to 5 dimensions typically costs +50-80% per operation.

**Measure the hot path, not I/O.** Replace network calls with channel sinks or in-memory buffers. You're measuring CPU/memory overhead, not network latency. The benchmark should be deterministic and fast.

**Local state, not global.** Each benchmark group creates its own context to avoid cross-contamination. Use large buffer capacities to prevent backpressure artifacts.

**`#[inline(never)]` on simulation functions.** Prevents the compiler from inlining across operation boundaries, which would produce unrealistically optimistic numbers.

**Committed baselines, not CI-only.** The TOML lives in the repo. Developers see it in PRs. The SVG renders in the README. Performance is visible, not hidden behind a CI dashboard.

**Separate timing from allocation measurement.** The global allocator tracking adds overhead that skews timing. Run timing benchmarks without tracking, then run allocation profiling separately.

## Interpreting Results

When comparing v1 → v2 of your benchmarks (e.g., adding more realistic dimensions):

- **+50-80% per operation** when going from 2 to 5 attribute dimensions is normal — it's the cost of sorting, hashing, and HashMap lookups per extra dimension
- **+100%+ on complex multi-step operations** is expected when adding child operations, each with their own attributes
- **Health/trivial endpoints** should show minimal regression (<25%) — they validate that the base overhead (context creation, span lifecycle) hasn't changed
- **Collect/snapshot operations** scale with total series count — +100% when doubling the number of active metric series is expected

The budget calculation is what matters: if weighted average overhead * target throughput stays within your CPU budget, individual operation regressions from more realistic parameters are acceptable — they reflect reality, not a performance bug.
