//! Reads `benches/baseline.toml` and renders `docs/illustration/performance.svg`.
//!
//! Usage: `cargo run --example render_baseline`

use std::collections::BTreeMap;
use std::fmt::Write;

use serde::Deserialize;

// ---------------------------------------------------------------------------
// TOML schema
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Baseline {
    metadata: Metadata,
    benchmarks: BTreeMap<String, Benchmark>,
    traffic_weights: BTreeMap<String, f64>,
    cpu_budget: CpuBudget,
}

#[derive(Deserialize)]
struct Metadata {
    version: String,
    date: String,
    platform: String,
    cpu: String,
    #[allow(dead_code)]
    rust_version: String,
    #[allow(dead_code)]
    notes: String,
}

#[derive(Deserialize)]
struct Benchmark {
    description: String,
    mean_ns: u64,
    #[allow(dead_code)]
    median_ns: u64,
    #[allow(dead_code)]
    allocations_approx: u64,
}

#[derive(Deserialize)]
struct CpuBudget {
    target_rps: u64,
    budget_percent: f64,
    budget_us_per_request: f64,
    #[allow(dead_code)]
    weighted_avg_us: f64,
    #[allow(dead_code)]
    total_cpu_percent: f64,
    #[allow(dead_code)]
    verdict: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const BUDGET_US: f64 = 33.3;

fn short_name(name: &str) -> &str {
    name.strip_prefix("ecommerce_").unwrap_or(name)
}

fn bar_color(us: f64) -> &'static str {
    let ratio = us / BUDGET_US;
    if ratio < 0.7 {
        "#059669" // green
    } else if ratio < 1.0 {
        "#ea580c" // orange
    } else {
        "#dc2626" // red
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let toml_str = std::fs::read_to_string("benches/baseline.toml")
        .expect("Failed to read benches/baseline.toml — run from repo root");
    let baseline: Baseline = toml::from_str(&toml_str).expect("Failed to parse baseline TOML");

    // Collect ordered data
    let endpoint_order = [
        "ecommerce_catalog_browse",
        "ecommerce_search",
        "ecommerce_product_detail",
        "ecommerce_cart_add",
        "ecommerce_checkout",
        "ecommerce_health",
    ];
    let entries: Vec<(&str, &Benchmark, f64)> = endpoint_order
        .iter()
        .filter_map(|name| {
            let bench = baseline.benchmarks.get(*name)?;
            let weight = baseline.traffic_weights.get(*name).copied().unwrap_or(0.0);
            Some((*name, bench, weight))
        })
        .collect();

    let all_zero = entries.iter().all(|(_, b, _)| b.mean_ns == 0);

    // Compute derived values
    let weighted_avg_us: f64 = entries
        .iter()
        .map(|(_, b, w)| (b.mean_ns as f64 / 1000.0) * w)
        .sum();
    let total_cpu_pct =
        (weighted_avg_us * baseline.cpu_budget.target_rps as f64) / 1_000_000.0 * 100.0;
    let verdict = if all_zero {
        "PENDING"
    } else if total_cpu_pct <= baseline.cpu_budget.budget_percent {
        "PASS"
    } else {
        "FAIL"
    };

    // Write computed values back to baseline.toml (preserves comments/formatting)
    {
        let mut updated = toml_str.clone();
        for (key, val) in [
            ("weighted_avg_us", format!("{:.3}", weighted_avg_us)),
            ("total_cpu_percent", format!("{:.2}", total_cpu_pct)),
            ("verdict", format!("\"{}\"", verdict)),
        ] {
            let prefix = format!("{} = ", key);
            if let Some(start) = updated.find(&prefix) {
                let val_start = start + prefix.len();
                let val_end = updated[val_start..]
                    .find('\n')
                    .map(|i| val_start + i)
                    .unwrap_or(updated.len());
                updated.replace_range(val_start..val_end, &val);
            }
        }
        std::fs::write("benches/baseline.toml", &updated)
            .expect("Failed to write benches/baseline.toml");
        println!(
            "Updated baseline.toml: weighted_avg_us={:.3}, total_cpu_percent={:.2}, verdict={}",
            weighted_avg_us, total_cpu_pct, verdict
        );
    }

    // Render hand-crafted SVG
    let svg = render_svg(&baseline, &entries, weighted_avg_us, total_cpu_pct, verdict);
    std::fs::write("docs/illustration/performance.svg", &svg)
        .expect("Failed to write performance.svg");
    println!("Rendered docs/illustration/performance.svg");
}

// ---------------------------------------------------------------------------
// SVG renderer
// ---------------------------------------------------------------------------

fn render_svg(
    baseline: &Baseline,
    entries: &[(&str, &Benchmark, f64)],
    weighted_avg_us: f64,
    total_cpu_pct: f64,
    verdict: &str,
) -> String {
    let width = 780;
    let row_h = 32;
    let header_y = 100;
    let table_top = header_y + row_h + 4;
    let n_rows = entries.len();
    let table_bottom = table_top + (n_rows as u32) * row_h;
    let summary_y = table_bottom + 16;
    let footer_y = summary_y + 60;
    let height = footer_y + 30;

    let max_us = entries
        .iter()
        .map(|(_, b, _)| b.mean_ns as f64 / 1000.0)
        .fold(0.0_f64, f64::max)
        .max(BUDGET_US);

    // Bar dimensions — fits inside the Latency column
    let bar_col_x: u32 = 200;
    let bar_max_w: u32 = 180;

    let mut svg = String::with_capacity(8192);

    // Preamble
    writeln!(
        svg,
        r##"<svg viewBox="0 0 {width} {height}" xmlns="http://www.w3.org/2000/svg">"##
    )
    .unwrap();
    writeln!(svg, "  <style>").unwrap();
    writeln!(
        svg,
        "    .title {{ font: bold 18px monospace; fill: #1e293b; }}"
    )
    .unwrap();
    writeln!(
        svg,
        "    .subtitle {{ font: 14px system-ui, -apple-system, sans-serif; fill: #64748b; }}"
    )
    .unwrap();
    writeln!(
        svg,
        "    .col-hdr {{ font: bold 10px monospace; fill: #64748b; }}"
    )
    .unwrap();
    writeln!(svg, "    .cell {{ font: 12px monospace; fill: #1e293b; }}").unwrap();
    writeln!(
        svg,
        "    .cell-desc {{ font: 10px system-ui, -apple-system, sans-serif; fill: #94a3b8; }}"
    )
    .unwrap();
    writeln!(
        svg,
        "    .summary {{ font: 12px monospace; fill: #1e293b; }}"
    )
    .unwrap();
    writeln!(svg, "    .verdict {{ font: bold 12px monospace; }}").unwrap();
    writeln!(
        svg,
        "    .footer {{ font: 10px system-ui, -apple-system, sans-serif; fill: #94a3b8; }}"
    )
    .unwrap();
    writeln!(svg, "  </style>").unwrap();

    // Canvas
    writeln!(
        svg,
        r##"  <rect width="{width}" height="{height}" fill="#fafafa"/>"##
    )
    .unwrap();

    // Surface card
    let card_x = 16;
    let card_y = 12;
    let card_w = width - 32;
    let card_h = height - 24;
    writeln!(svg, r##"  <rect x="{card_x}" y="{card_y}" width="{card_w}" height="{card_h}" rx="8" fill="#ffffff" stroke="#e2e8f0" stroke-width="1"/>"##).unwrap();

    // Accent line
    writeln!(
        svg,
        r##"  <rect x="{card_x}" y="{card_y}" width="{card_w}" height="4" rx="8" fill="#ea580c"/>"##
    )
    .unwrap();

    // Title
    let cx = width / 2;
    writeln!(svg, r##"  <text x="{cx}" y="42" text-anchor="middle" class="title">rolly v{} Performance Baseline</text>"##, xml_escape(&baseline.metadata.version)).unwrap();

    // Subtitle
    writeln!(
        svg,
        r##"  <text x="{cx}" y="62" text-anchor="middle" class="subtitle">{} / {} / {}</text>"##,
        xml_escape(&baseline.metadata.platform),
        xml_escape(&baseline.metadata.cpu),
        xml_escape(&baseline.metadata.date)
    )
    .unwrap();

    // Separator
    writeln!(
        svg,
        r##"  <line x1="32" y1="76" x2="{}" y2="76" stroke="#e2e8f0" stroke-width="1"/>"##,
        width - 32
    )
    .unwrap();

    // Column headers
    let hy = header_y - 8;
    writeln!(
        svg,
        r##"  <text x="36" y="{hy}" class="col-hdr">ENDPOINT</text>"##
    )
    .unwrap();
    writeln!(
        svg,
        r##"  <text x="{bar_col_x}" y="{hy}" class="col-hdr">LATENCY (us)</text>"##
    )
    .unwrap();
    writeln!(
        svg,
        r##"  <text x="400" y="{hy}" class="col-hdr">WEIGHT</text>"##
    )
    .unwrap();
    writeln!(
        svg,
        r##"  <text x="460" y="{hy}" class="col-hdr">ALLOCS</text>"##
    )
    .unwrap();
    writeln!(
        svg,
        r##"  <text x="520" y="{hy}" class="col-hdr">DESCRIPTION</text>"##
    )
    .unwrap();

    // Header separator
    writeln!(
        svg,
        r##"  <line x1="32" y1="{}" x2="{}" y2="{}" stroke="#e2e8f0" stroke-width="0.5"/>"##,
        header_y - 2,
        width - 32,
        header_y - 2
    )
    .unwrap();

    // Table rows
    for (i, (name, bench, weight)) in entries.iter().enumerate() {
        let us = bench.mean_ns as f64 / 1000.0;
        let y = table_top + (i as u32) * row_h;
        let text_y = y + 14;
        let desc_y = y + 25;

        // Alternating row background
        if i % 2 == 0 {
            writeln!(
                svg,
                r##"  <rect x="32" y="{y}" width="{}" height="{row_h}" fill="#f8fafc"/>"##,
                width - 64
            )
            .unwrap();
        }

        // Endpoint name
        writeln!(
            svg,
            r##"  <text x="36" y="{text_y}" class="cell">{}</text>"##,
            xml_escape(short_name(name))
        )
        .unwrap();

        // Latency bar
        let bar_w = if max_us > 0.0 {
            ((us / max_us) * bar_max_w as f64) as u32
        } else {
            0
        };
        let bar_y = y + 4;
        let color = bar_color(us);
        writeln!(svg, r##"  <rect x="{bar_col_x}" y="{bar_y}" width="{bar_w}" height="14" rx="2" fill="{color}" opacity="0.8"/>"##).unwrap();

        // Latency value
        let val_x = bar_col_x + bar_w + 4;
        writeln!(
            svg,
            r##"  <text x="{val_x}" y="{text_y}" class="cell">{:.1}</text>"##,
            us
        )
        .unwrap();

        // Weight
        writeln!(
            svg,
            r##"  <text x="400" y="{text_y}" class="cell">{:.0}%</text>"##,
            weight * 100.0
        )
        .unwrap();

        // Allocs
        writeln!(
            svg,
            r##"  <text x="460" y="{text_y}" class="cell">{}</text>"##,
            bench.allocations_approx
        )
        .unwrap();

        // Description (truncated to fit)
        let desc = if bench.description.len() > 40 {
            format!("{}...", &bench.description[..37])
        } else {
            bench.description.clone()
        };
        writeln!(
            svg,
            r##"  <text x="520" y="{desc_y}" class="cell-desc">{}</text>"##,
            xml_escape(&desc)
        )
        .unwrap();

        // Row separator
        let sep_y = y + row_h;
        writeln!(svg, r##"  <line x1="32" y1="{sep_y}" x2="{}" y2="{sep_y}" stroke="#e2e8f0" stroke-width="0.5"/>"##, width - 32).unwrap();
    }

    // Budget line in the bar column (vertical dashed line)
    let budget_bar_x =
        bar_col_x + ((BUDGET_US / max_us) * bar_max_w as f64).min(bar_max_w as f64) as u32;
    writeln!(svg, r##"  <line x1="{budget_bar_x}" y1="{table_top}" x2="{budget_bar_x}" y2="{table_bottom}" stroke="#dc2626" stroke-width="1" stroke-dasharray="4,2"/>"##).unwrap();

    // Summary section
    let (verdict_color, verdict_bg) = if verdict == "PASS" {
        ("#059669", "#ecfdf5")
    } else if verdict == "FAIL" {
        ("#dc2626", "#fef2f2")
    } else {
        ("#64748b", "#f1f5f9")
    };

    // Summary separator
    writeln!(
        svg,
        r##"  <line x1="32" y1="{}" x2="{}" y2="{}" stroke="#e2e8f0" stroke-width="1"/>"##,
        summary_y - 4,
        width - 32,
        summary_y - 4
    )
    .unwrap();

    writeln!(
        svg,
        r##"  <text x="36" y="{}" class="summary">Weighted avg: {:.2} us/req</text>"##,
        summary_y + 14,
        weighted_avg_us
    )
    .unwrap();
    writeln!(
        svg,
        r##"  <text x="250" y="{}" class="summary">CPU at {} req/s: {:.2}%</text>"##,
        summary_y + 14,
        baseline.cpu_budget.target_rps,
        total_cpu_pct
    )
    .unwrap();

    // Verdict badge
    let badge_x = 500;
    let badge_y = summary_y + 4;
    writeln!(svg, r##"  <rect x="{badge_x}" y="{badge_y}" width="70" height="20" rx="10" fill="{verdict_bg}" stroke="{verdict_color}" stroke-width="1"/>"##).unwrap();
    writeln!(svg, r##"  <text x="{}" y="{}" text-anchor="middle" class="verdict" fill="{verdict_color}">{verdict}</text>"##,
        badge_x + 35, badge_y + 14
    ).unwrap();

    // Footer
    writeln!(svg, r##"  <text x="{cx}" y="{}" text-anchor="middle" class="footer">Budget: {:.1} us/req ({}% CPU at {} req/s)</text>"##,
        footer_y,
        baseline.cpu_budget.budget_us_per_request,
        baseline.cpu_budget.budget_percent,
        baseline.cpu_budget.target_rps
    ).unwrap();
    writeln!(svg, r##"  <text x="{cx}" y="{}" text-anchor="middle" class="footer">rolly &#x2014; github.com/l1x/rolly</text>"##,
        footer_y + 16
    ).unwrap();

    writeln!(svg, "</svg>").unwrap();
    svg
}
