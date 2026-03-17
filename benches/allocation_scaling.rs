//! Allocation-scaling benchmark: measures total heap allocations at multiple N
//! values for both rolly and OTel, asserts that allocs/op stays constant
//! (linear scaling), and catches accidental quadratic memory growth.
//!
//! This is a separate bench binary so the `#[global_allocator]` override
//! doesn't affect other benchmarks' timing.
//!
//! Run: `cargo bench --features _bench --bench allocation_scaling`

use std::alloc::System;
use std::hint::black_box;
use std::process::ExitCode;
use std::time::Instant;

use stats_alloc::{Region, StatsAlloc, INSTRUMENTED_SYSTEM};

use rolly::bench::*;

use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::{metrics::Meter, KeyValue};
use opentelemetry_sdk::metrics::{ManualReader, SdkMeterProvider};

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

// ---------------------------------------------------------------------------
// N values — 100x span catches quadratic growth as 100x allocs/op increase
// ---------------------------------------------------------------------------
const NS: [usize; 3] = [100, 1_000, 10_000];

// Scaling tolerance: allocs/op at max N must be within this factor of min N
const SCALING_TOLERANCE: f64 = 1.5;

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

struct ScalingResult {
    n: usize,
    allocations: usize,
    bytes: usize,
    duration_ns: u64,
}

impl ScalingResult {
    fn allocs_per_op(&self) -> f64 {
        self.allocations as f64 / self.n as f64
    }

    fn bytes_per_op(&self) -> f64 {
        self.bytes as f64 / self.n as f64
    }

    fn ns_per_op(&self) -> f64 {
        self.duration_ns as f64 / self.n as f64
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScenarioKind {
    Hot,
    Cold,
    Mixed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lib {
    Rolly,
    OTel,
}

#[allow(dead_code)]
struct ScenarioResults {
    name: String,
    kind: ScenarioKind,
    lib: Lib,
    results: Vec<ScalingResult>,
}

// ---------------------------------------------------------------------------
// OTel helpers
// ---------------------------------------------------------------------------

fn otel_provider() -> SdkMeterProvider {
    SdkMeterProvider::builder()
        .with_reader(ManualReader::builder().build())
        .build()
}

fn otel_meter(provider: &SdkMeterProvider) -> Meter {
    provider.meter("bench")
}

// ---------------------------------------------------------------------------
// rolly hot-path scenarios
// ---------------------------------------------------------------------------

fn measure_rolly_counter_hot(n: usize) -> ScalingResult {
    let registry = MetricsRegistry::new();
    let counter = registry.counter("requests", "total requests");
    let attrs: &[(&str, &str)] = &[
        ("method", "GET"),
        ("status", "200"),
        ("region", "us-east-1"),
    ];
    // warmup — first insert allocates
    counter.add(1, attrs);

    let reg = Region::new(GLOBAL);
    let start = Instant::now();
    for _ in 0..n {
        counter.add(black_box(1), black_box(attrs));
    }
    let elapsed = start.elapsed();
    let stats = reg.change();

    ScalingResult {
        n,
        allocations: stats.allocations,
        bytes: stats.bytes_allocated,
        duration_ns: elapsed.as_nanos() as u64,
    }
}

fn measure_rolly_gauge_hot(n: usize) -> ScalingResult {
    let registry = MetricsRegistry::new();
    let gauge = registry.gauge("connections", "active connections");
    let attrs: &[(&str, &str)] = &[
        ("method", "GET"),
        ("status", "200"),
        ("region", "us-east-1"),
    ];
    gauge.set(1.0, attrs);

    let reg = Region::new(GLOBAL);
    let start = Instant::now();
    for _ in 0..n {
        gauge.set(black_box(42.0), black_box(attrs));
    }
    let elapsed = start.elapsed();
    let stats = reg.change();

    ScalingResult {
        n,
        allocations: stats.allocations,
        bytes: stats.bytes_allocated,
        duration_ns: elapsed.as_nanos() as u64,
    }
}

fn measure_rolly_histogram_hot(n: usize) -> ScalingResult {
    let registry = MetricsRegistry::new();
    let hist = registry.histogram(
        "request_duration",
        "HTTP request duration",
        &[5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0],
    );
    let attrs: &[(&str, &str)] = &[
        ("method", "GET"),
        ("status", "200"),
        ("region", "us-east-1"),
    ];
    hist.observe(42.5, attrs);

    let reg = Region::new(GLOBAL);
    let start = Instant::now();
    for _ in 0..n {
        hist.observe(black_box(42.5), black_box(attrs));
    }
    let elapsed = start.elapsed();
    let stats = reg.change();

    ScalingResult {
        n,
        allocations: stats.allocations,
        bytes: stats.bytes_allocated,
        duration_ns: elapsed.as_nanos() as u64,
    }
}

// ---------------------------------------------------------------------------
// rolly cold-path scenarios
// ---------------------------------------------------------------------------

fn measure_rolly_counter_cold(n: usize) -> ScalingResult {
    let registry = MetricsRegistry::new();
    // Raise cardinality limit so N=10000 doesn't get capped
    let counter = registry.counter_with_max_cardinality("requests", "total requests", n + 100);

    let reg = Region::new(GLOBAL);
    let start = Instant::now();
    for i in 0..n {
        let status = format!("{i}");
        counter.add(
            black_box(1),
            black_box(&[
                ("method", "GET"),
                ("status", status.as_str()),
                ("region", "us-east-1"),
            ]),
        );
    }
    let elapsed = start.elapsed();
    let stats = reg.change();

    ScalingResult {
        n,
        allocations: stats.allocations,
        bytes: stats.bytes_allocated,
        duration_ns: elapsed.as_nanos() as u64,
    }
}

fn measure_rolly_counter_mixed(n: usize) -> ScalingResult {
    let registry = MetricsRegistry::new();
    let counter = registry.counter_with_max_cardinality("requests", "total requests", n + 100);
    let hot_attrs: &[(&str, &str)] = &[
        ("method", "GET"),
        ("status", "200"),
        ("region", "us-east-1"),
    ];
    // warmup hot path
    counter.add(1, hot_attrs);

    let reg = Region::new(GLOBAL);
    let start = Instant::now();
    for i in 0..n {
        if i % 10 == 0 {
            // 10% cold — distinct attr set each time
            let status = format!("cold_{i}");
            counter.add(
                black_box(1),
                black_box(&[
                    ("method", "POST"),
                    ("status", status.as_str()),
                    ("region", "us-east-1"),
                ]),
            );
        } else {
            // 90% hot — reuse same attrs
            counter.add(black_box(1), black_box(hot_attrs));
        }
    }
    let elapsed = start.elapsed();
    let stats = reg.change();

    ScalingResult {
        n,
        allocations: stats.allocations,
        bytes: stats.bytes_allocated,
        duration_ns: elapsed.as_nanos() as u64,
    }
}

// ---------------------------------------------------------------------------
// OTel scenarios
// ---------------------------------------------------------------------------

fn measure_otel_counter_hot(n: usize) -> ScalingResult {
    let provider = otel_provider();
    let meter = otel_meter(&provider);
    let counter = meter.u64_counter("requests").build();
    let attrs = [
        KeyValue::new("method", "GET"),
        KeyValue::new("status", "200"),
        KeyValue::new("region", "us-east-1"),
    ];
    // warmup
    counter.add(1, &attrs);

    let reg = Region::new(GLOBAL);
    let start = Instant::now();
    for _ in 0..n {
        counter.add(black_box(1), black_box(&attrs));
    }
    let elapsed = start.elapsed();
    let stats = reg.change();

    ScalingResult {
        n,
        allocations: stats.allocations,
        bytes: stats.bytes_allocated,
        duration_ns: elapsed.as_nanos() as u64,
    }
}

fn measure_otel_counter_cold(n: usize) -> ScalingResult {
    let provider = otel_provider();
    let meter = otel_meter(&provider);
    let counter = meter.u64_counter("requests").build();

    let reg = Region::new(GLOBAL);
    let start = Instant::now();
    for i in 0..n {
        counter.add(
            black_box(1),
            black_box(&[
                KeyValue::new("method", "GET"),
                KeyValue::new("status", format!("{i}")),
                KeyValue::new("region", "us-east-1"),
            ]),
        );
    }
    let elapsed = start.elapsed();
    let stats = reg.change();

    ScalingResult {
        n,
        allocations: stats.allocations,
        bytes: stats.bytes_allocated,
        duration_ns: elapsed.as_nanos() as u64,
    }
}

fn measure_otel_counter_mixed(n: usize) -> ScalingResult {
    let provider = otel_provider();
    let meter = otel_meter(&provider);
    let counter = meter.u64_counter("requests").build();
    let hot_attrs = [
        KeyValue::new("method", "GET"),
        KeyValue::new("status", "200"),
        KeyValue::new("region", "us-east-1"),
    ];
    // warmup
    counter.add(1, &hot_attrs);

    let reg = Region::new(GLOBAL);
    let start = Instant::now();
    for i in 0..n {
        if i % 10 == 0 {
            // 10% cold
            counter.add(
                black_box(1),
                black_box(&[
                    KeyValue::new("method", "POST"),
                    KeyValue::new("status", format!("cold_{i}")),
                    KeyValue::new("region", "us-east-1"),
                ]),
            );
        } else {
            // 90% hot
            counter.add(black_box(1), black_box(&hot_attrs));
        }
    }
    let elapsed = start.elapsed();
    let stats = reg.change();

    ScalingResult {
        n,
        allocations: stats.allocations,
        bytes: stats.bytes_allocated,
        duration_ns: elapsed.as_nanos() as u64,
    }
}

// ---------------------------------------------------------------------------
// Printing & assertions
// ---------------------------------------------------------------------------

fn print_header() {
    println!(
        "  {:>8}  {:>12}  {:>12}  {:>12}  {:>12}  {:>12}",
        "N", "allocs", "bytes", "allocs/op", "bytes/op", "ns/op"
    );
}

fn print_row(r: &ScalingResult) {
    println!(
        "  {:>8}  {:>12}  {:>12}  {:>12.2}  {:>12.1}  {:>12.1}",
        fmt_num(r.n),
        fmt_num(r.allocations),
        fmt_num(r.bytes),
        r.allocs_per_op(),
        r.bytes_per_op(),
        r.ns_per_op(),
    );
}

fn fmt_num(n: usize) -> String {
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

/// Assert zero allocations on hot path at every N.
fn assert_zero_alloc(name: &str, results: &[ScalingResult]) -> bool {
    print_header();
    for r in results {
        print_row(r);
    }

    let all_zero = results.iter().all(|r| r.allocations == 0);
    if all_zero {
        println!("  PASS: zero allocations on hot path.\n");
    } else {
        println!("  FAIL: expected zero allocations on hot path!\n");
        eprintln!("FAIL: {name}");
    }
    all_zero
}

/// Assert allocs/op stays within tolerance across N values (linear scaling).
fn assert_linear_scaling(name: &str, results: &[ScalingResult]) -> bool {
    print_header();
    for r in results {
        print_row(r);
    }

    let first = results.first().unwrap().allocs_per_op();
    let last = results.last().unwrap().allocs_per_op();

    // Guard against division by zero when first is 0
    let pass = if first == 0.0 {
        last == 0.0
    } else {
        (last / first) <= SCALING_TOLERANCE
    };

    if pass {
        println!("  PASS: allocs/op stable across N values.\n");
    } else {
        println!(
            "  FAIL: allocs/op ratio {:.2}x exceeds {:.1}x tolerance!\n",
            last / first,
            SCALING_TOLERANCE,
        );
        eprintln!("FAIL: {name}");
    }
    pass
}

// ---------------------------------------------------------------------------
// SVG rendering — clean table layout
// ---------------------------------------------------------------------------

struct TableRow {
    scenario: &'static str,
    rolly_allocs: f64,
    otel_allocs: Option<f64>,
    rolly_ns: f64,
    otel_ns: Option<f64>,
}

fn render_svg(scenarios: &[ScenarioResults]) {
    use std::fmt::Write;

    // Pair rolly scenarios with their OTel counterparts at N=10,000
    let pairs: &[(&str, &str, Option<&str>)] = &[
        (
            "counter (hot)",
            "rolly counter (hot)",
            Some("OTel counter (hot)"),
        ),
        ("gauge (hot)", "rolly gauge (hot)", None),
        ("histogram (hot)", "rolly histogram (hot)", None),
        (
            "counter (cold)",
            "rolly counter (cold)",
            Some("OTel counter (cold)"),
        ),
        (
            "counter (mixed)",
            "rolly counter (mixed)",
            Some("OTel counter (mixed)"),
        ),
    ];

    let find = |name: &str| -> Option<&ScalingResult> {
        scenarios
            .iter()
            .find(|s| s.name == name)
            .and_then(|s| s.results.last())
    };

    let rows: Vec<TableRow> = pairs
        .iter()
        .map(|(label, rolly_name, otel_name)| {
            let rolly = find(rolly_name).unwrap();
            let otel = otel_name.and_then(&find);
            TableRow {
                scenario: label,
                rolly_allocs: rolly.allocs_per_op(),
                otel_allocs: otel.map(|r| r.allocs_per_op()),
                rolly_ns: rolly.ns_per_op(),
                otel_ns: otel.map(|r| r.ns_per_op()),
            }
        })
        .collect();

    // Layout constants
    let w = 820;
    let row_h = 40;
    let header_rows = 2;
    let data_rows = rows.len();
    let table_y = 70;
    let table_h = (header_rows + data_rows) * row_h;
    let svg_h = table_y + table_h + 60;
    let table_x = 40;

    // Column positions (x offsets from table_x)
    let col_scenario_w = 200;
    let col_val_w = 120;
    let col_allocs_x = col_scenario_w;
    let col_allocs_otel_x = col_allocs_x + col_val_w;
    let col_ns_x = col_allocs_otel_x + col_val_w;
    let col_ns_otel_x = col_ns_x + col_val_w;
    let table_w = col_ns_otel_x + col_val_w;

    let green = "#ecfdf5"; // light green bg for winner
    let blue_bg = "#eff6ff"; // light blue bg for winner
    let header_bg = "#fff";
    let header_bg2 = "#fff";
    let header_fg = "#1e293b";
    let white = "#ffffff";
    let alt_row = "#fafafa";
    let border = "#e2e8f0";
    let text_color = "#1e293b";
    let subtitle_color = "#64748b";
    let header_line = "#e2e8f0";
    let legend_text = "#64748b";
    let legend_muted = "#94a3b8";
    let accent = "#ea580c";

    let mut svg = String::with_capacity(4096);
    writeln!(svg, "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{svg_h}\">").unwrap();
    writeln!(svg, "<rect width=\"{w}\" height=\"{svg_h}\" fill=\"#fafafa\"/>").unwrap();

    // Title
    writeln!(svg, "<text x=\"{}\" y=\"35\" font-size=\"18\" font-weight=\"bold\" text-anchor=\"middle\" fill=\"{text_color}\" font-family=\"monospace\">Allocation Scaling &#x2014; rolly vs OTel (N = 10,000)</text>", w / 2).unwrap();
    writeln!(svg, "<text x=\"{}\" y=\"55\" font-size=\"12\" fill=\"{subtitle_color}\" text-anchor=\"middle\" font-family=\"system-ui, -apple-system, sans-serif\">Lower is better. Winner highlighted.</text>", w / 2).unwrap();

    // Table border
    writeln!(svg, "<rect x=\"{table_x}\" y=\"{table_y}\" width=\"{table_w}\" height=\"{table_h}\" fill=\"{white}\" stroke=\"{border}\" stroke-width=\"1\" rx=\"4\"/>").unwrap();
    // Accent line at top
    writeln!(svg, "<rect x=\"{table_x}\" y=\"{table_y}\" width=\"{table_w}\" height=\"4\" fill=\"{accent}\" rx=\"4\"/>").unwrap();
    writeln!(svg, "<rect x=\"{table_x}\" y=\"{}\" width=\"{table_w}\" height=\"2\" fill=\"{accent}\"/>", table_y + 2).unwrap();

    // --- Header row 1: merged group headers ---
    let hy = table_y;
    writeln!(svg, "<rect x=\"{table_x}\" y=\"{hy}\" width=\"{table_w}\" height=\"{row_h}\" fill=\"{header_bg}\" rx=\"4\"/>").unwrap();
    // Fix bottom corners (cover the rounded bottom)
    writeln!(
        svg,
        "<rect x=\"{table_x}\" y=\"{}\" width=\"{table_w}\" height=\"4\" fill=\"{header_bg}\"/>",
        hy + row_h - 4
    )
    .unwrap();

    let scenario_cx = table_x + col_scenario_w / 2;
    let allocs_cx = table_x + col_allocs_x + col_val_w; // center of 2 columns
    let ns_cx = table_x + col_ns_x + col_val_w;
    let ty = hy + 26;
    writeln!(svg, "<text x=\"{scenario_cx}\" y=\"{ty}\" font-size=\"12\" font-weight=\"bold\" fill=\"{header_fg}\" text-anchor=\"middle\" font-family=\"monospace\">Scenario</text>").unwrap();
    writeln!(svg, "<text x=\"{allocs_cx}\" y=\"{ty}\" font-size=\"12\" font-weight=\"bold\" fill=\"{header_fg}\" text-anchor=\"middle\" font-family=\"monospace\">allocs / op</text>").unwrap();
    writeln!(svg, "<text x=\"{ns_cx}\" y=\"{ty}\" font-size=\"12\" font-weight=\"bold\" fill=\"{header_fg}\" text-anchor=\"middle\" font-family=\"monospace\">ns / op</text>").unwrap();

    // Vertical separator lines in header
    for &cx in &[col_allocs_x, col_allocs_otel_x, col_ns_x, col_ns_otel_x] {
        let lx = table_x + cx;
        writeln!(svg, "<line x1=\"{lx}\" y1=\"{hy}\" x2=\"{lx}\" y2=\"{}\" stroke=\"{header_line}\" stroke-width=\"1\"/>", hy + row_h * 2).unwrap();
    }

    // --- Header row 2: sub-headers ---
    let hy2 = hy + row_h;
    writeln!(svg, "<rect x=\"{table_x}\" y=\"{hy2}\" width=\"{table_w}\" height=\"{row_h}\" fill=\"{header_bg2}\"/>").unwrap();
    writeln!(svg, "<line x1=\"{table_x}\" y1=\"{hy2}\" x2=\"{}\" y2=\"{hy2}\" stroke=\"{border}\" stroke-width=\"1\"/>", table_x + table_w).unwrap();
    let ty2 = hy2 + 26;
    for (col_x, label) in [
        (col_allocs_x, "rolly"),
        (col_allocs_otel_x, "OTel"),
        (col_ns_x, "rolly"),
        (col_ns_otel_x, "OTel"),
    ] {
        let cx = table_x + col_x + col_val_w / 2;
        writeln!(svg, "<text x=\"{cx}\" y=\"{ty2}\" font-size=\"12\" font-weight=\"bold\" fill=\"{header_fg}\" text-anchor=\"middle\" font-family=\"monospace\">{label}</text>").unwrap();
    }

    // --- Data rows ---
    for (i, row) in rows.iter().enumerate() {
        let ry = table_y + (header_rows + i) * row_h;
        let bg = if i % 2 == 1 { alt_row } else { white };
        writeln!(svg, "<rect x=\"{table_x}\" y=\"{ry}\" width=\"{table_w}\" height=\"{row_h}\" fill=\"{bg}\"/>").unwrap();

        // Highlight winner cells
        if let Some(otel_a) = row.otel_allocs {
            let (win_x, win_bg) = if row.rolly_allocs <= otel_a {
                (col_allocs_x, green)
            } else {
                (col_allocs_otel_x, blue_bg)
            };
            writeln!(svg, "<rect x=\"{}\" y=\"{ry}\" width=\"{col_val_w}\" height=\"{row_h}\" fill=\"{win_bg}\"/>", table_x + win_x).unwrap();
        }
        if let Some(otel_n) = row.otel_ns {
            let (win_x, win_bg) = if row.rolly_ns <= otel_n {
                (col_ns_x, green)
            } else {
                (col_ns_otel_x, blue_bg)
            };
            writeln!(svg, "<rect x=\"{}\" y=\"{ry}\" width=\"{col_val_w}\" height=\"{row_h}\" fill=\"{win_bg}\"/>", table_x + win_x).unwrap();
        }

        let ty = ry + 26;

        // Scenario name
        writeln!(
            svg,
            "<text x=\"{}\" y=\"{ty}\" font-size=\"12\" fill=\"{text_color}\" font-family=\"monospace\">{}</text>",
            table_x + 12,
            row.scenario
        )
        .unwrap();

        // Values
        let fmt_allocs = |v: f64| -> String {
            if v == 0.0 {
                "0".to_string()
            } else {
                format!("{:.2}", v)
            }
        };
        let fmt_ns = |v: f64| -> String { format!("{:.1}", v) };

        let vals: [(usize, String); 4] = [
            (col_allocs_x, fmt_allocs(row.rolly_allocs)),
            (
                col_allocs_otel_x,
                row.otel_allocs.map_or("\u{2014}".to_string(), fmt_allocs),
            ),
            (col_ns_x, fmt_ns(row.rolly_ns)),
            (
                col_ns_otel_x,
                row.otel_ns.map_or("\u{2014}".to_string(), fmt_ns),
            ),
        ];
        for (col_x, val) in &vals {
            let cx = table_x + col_x + col_val_w - 16;
            writeln!(svg, "<text x=\"{cx}\" y=\"{ty}\" font-size=\"12\" fill=\"{text_color}\" text-anchor=\"end\" font-family=\"monospace\">{val}</text>").unwrap();
        }

        // Horizontal row separator
        writeln!(svg, "<line x1=\"{table_x}\" y1=\"{ry}\" x2=\"{}\" y2=\"{ry}\" stroke=\"{border}\" stroke-width=\"0.5\"/>", table_x + table_w).unwrap();
    }

    // Column separator lines through data area
    let data_top = table_y + header_rows * row_h;
    let data_bottom = table_y + table_h;
    for &cx in &[col_allocs_x, col_allocs_otel_x, col_ns_x, col_ns_otel_x] {
        let lx = table_x + cx;
        writeln!(svg, "<line x1=\"{lx}\" y1=\"{data_top}\" x2=\"{lx}\" y2=\"{data_bottom}\" stroke=\"{border}\" stroke-width=\"0.5\"/>").unwrap();
    }

    // Legend
    let ly = table_y + table_h + 25;
    writeln!(svg, "<rect x=\"{}\" y=\"{}\" width=\"14\" height=\"14\" fill=\"{green}\" stroke=\"{border}\" stroke-width=\"0.5\" rx=\"2\"/>", table_x, ly - 12).unwrap();
    writeln!(
        svg,
        "<text x=\"{}\" y=\"{ly}\" font-size=\"10\" fill=\"{legend_text}\" font-family=\"system-ui, -apple-system, sans-serif\">rolly wins</text>",
        table_x + 20
    )
    .unwrap();
    writeln!(svg, "<rect x=\"{}\" y=\"{}\" width=\"14\" height=\"14\" fill=\"{blue_bg}\" stroke=\"{border}\" stroke-width=\"0.5\" rx=\"2\"/>", table_x + 120, ly - 12).unwrap();
    writeln!(
        svg,
        "<text x=\"{}\" y=\"{ly}\" font-size=\"10\" fill=\"{legend_text}\" font-family=\"system-ui, -apple-system, sans-serif\">OTel wins</text>",
        table_x + 140
    )
    .unwrap();
    writeln!(
        svg,
        "<text x=\"{}\" y=\"{ly}\" font-size=\"10\" fill=\"{legend_muted}\" font-family=\"system-ui, -apple-system, sans-serif\">Lower is better</text>",
        table_x + 240
    )
    .unwrap();

    writeln!(svg, "</svg>").unwrap();

    std::fs::write("docs/illustration/allocation-scaling.svg", &svg).unwrap();
    println!("Rendered docs/illustration/allocation-scaling.svg");
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    println!("\n=== Allocation Scaling ===\n");

    let mut all_pass = true;
    let mut all_scenarios: Vec<ScenarioResults> = Vec::new();

    // -- rolly counter hot path --
    {
        let name = "rolly counter (hot)";
        println!("--- {name} ---");
        let results: Vec<_> = NS.iter().map(|&n| measure_rolly_counter_hot(n)).collect();
        all_pass &= assert_zero_alloc(name, &results);
        all_scenarios.push(ScenarioResults {
            name: name.to_string(),
            kind: ScenarioKind::Hot,
            lib: Lib::Rolly,
            results,
        });
    }

    // -- rolly gauge hot path --
    {
        let name = "rolly gauge (hot)";
        println!("--- {name} ---");
        let results: Vec<_> = NS.iter().map(|&n| measure_rolly_gauge_hot(n)).collect();
        all_pass &= assert_zero_alloc(name, &results);
        all_scenarios.push(ScenarioResults {
            name: name.to_string(),
            kind: ScenarioKind::Hot,
            lib: Lib::Rolly,
            results,
        });
    }

    // -- rolly histogram hot path --
    {
        let name = "rolly histogram (hot)";
        println!("--- {name} ---");
        let results: Vec<_> = NS.iter().map(|&n| measure_rolly_histogram_hot(n)).collect();
        all_pass &= assert_zero_alloc(name, &results);
        all_scenarios.push(ScenarioResults {
            name: name.to_string(),
            kind: ScenarioKind::Hot,
            lib: Lib::Rolly,
            results,
        });
    }

    // -- rolly counter cold path --
    {
        let name = "rolly counter (cold)";
        println!("--- {name} ---");
        let results: Vec<_> = NS.iter().map(|&n| measure_rolly_counter_cold(n)).collect();
        all_pass &= assert_linear_scaling(name, &results);
        all_scenarios.push(ScenarioResults {
            name: name.to_string(),
            kind: ScenarioKind::Cold,
            lib: Lib::Rolly,
            results,
        });
    }

    // -- rolly counter mixed path --
    {
        let name = "rolly counter (mixed)";
        println!("--- {name} ---");
        let results: Vec<_> = NS.iter().map(|&n| measure_rolly_counter_mixed(n)).collect();
        all_pass &= assert_linear_scaling(name, &results);
        all_scenarios.push(ScenarioResults {
            name: name.to_string(),
            kind: ScenarioKind::Mixed,
            lib: Lib::Rolly,
            results,
        });
    }

    // -- OTel counter hot path --
    {
        let name = "OTel counter (hot)";
        println!("--- {name} ---");
        let results: Vec<_> = NS.iter().map(|&n| measure_otel_counter_hot(n)).collect();
        // OTel may or may not be zero-alloc; just report scaling
        all_pass &= assert_linear_scaling(name, &results);
        all_scenarios.push(ScenarioResults {
            name: name.to_string(),
            kind: ScenarioKind::Hot,
            lib: Lib::OTel,
            results,
        });
    }

    // -- OTel counter cold path --
    {
        let name = "OTel counter (cold)";
        println!("--- {name} ---");
        let results: Vec<_> = NS.iter().map(|&n| measure_otel_counter_cold(n)).collect();
        all_pass &= assert_linear_scaling(name, &results);
        all_scenarios.push(ScenarioResults {
            name: name.to_string(),
            kind: ScenarioKind::Cold,
            lib: Lib::OTel,
            results,
        });
    }

    // -- OTel counter mixed path --
    {
        let name = "OTel counter (mixed)";
        println!("--- {name} ---");
        let results: Vec<_> = NS.iter().map(|&n| measure_otel_counter_mixed(n)).collect();
        all_pass &= assert_linear_scaling(name, &results);
        all_scenarios.push(ScenarioResults {
            name: name.to_string(),
            kind: ScenarioKind::Mixed,
            lib: Lib::OTel,
            results,
        });
    }

    // Render SVG visualization
    render_svg(&all_scenarios);

    if all_pass {
        println!("All allocation scaling checks passed.");
        ExitCode::SUCCESS
    } else {
        eprintln!("\nSome allocation scaling checks FAILED.");
        ExitCode::FAILURE
    }
}
