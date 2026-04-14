//! USE metrics collector — reads `/proc/self/stat` and `/proc/self/statm` on Linux.
//!
//! Records `process.cpu.utilization` and `process.memory.usage` as registry-backed
//! gauges, exported as OTLP metrics. No-op on non-Linux platforms.

/// Stateful baseline for USE metrics polling.
///
/// Carries the previous CPU sample needed to compute utilization as a delta.
/// The first successful poll establishes the baseline; CPU utilization is
/// emitted starting on the second poll.
#[derive(Default)]
pub struct UseMetricsState {
    #[cfg(target_os = "linux")]
    prev_cpu_ticks: Option<u64>,
    #[cfg(target_os = "linux")]
    prev_instant: Option<std::time::Instant>,
    #[cfg(target_os = "linux")]
    cpu_gauge: Option<crate::Gauge>,
    #[cfg(target_os = "linux")]
    mem_gauge: Option<crate::Gauge>,
}

/// Poll USE metrics once.
///
/// Reads `/proc/self/stat` and `/proc/self/statm` synchronously, updates
/// `state`, and records gauge values in the global metrics registry.
/// The caller owns scheduling.
///
/// Only meaningful on Linux; returns immediately on other platforms.
pub(crate) fn poll_once(state: &mut UseMetricsState) {
    #[cfg(target_os = "linux")]
    {
        poll_once_linux(state);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = state;
    }
}

#[cfg(target_os = "linux")]
fn poll_once_linux(state: &mut UseMetricsState) {
    let now = std::time::Instant::now();

    let page_size = match page_size_bytes() {
        Some(ps) => ps,
        None => return,
    };
    let clock_ticks = match clock_ticks_per_sec() {
        Some(ct) => ct,
        None => return,
    };

    // Lazily create gauges on first poll
    let cpu_gauge = state.cpu_gauge.get_or_insert_with(|| {
        crate::gauge(
            "process.cpu.utilization",
            "Process CPU utilization (0.0–1.0+)",
        )
    });
    let mem_gauge = state.mem_gauge.get_or_insert_with(|| {
        crate::gauge("process.memory.usage", "Process resident set size in bytes")
    });

    if let Some((utime, stime)) = read_cpu_ticks() {
        let total_ticks = utime + stime;
        if let (Some(prev_ticks), Some(prev_instant)) = (state.prev_cpu_ticks, state.prev_instant) {
            let elapsed_secs = now.duration_since(prev_instant).as_secs_f64();
            let delta_ticks = total_ticks.saturating_sub(prev_ticks);
            let cpu_seconds = delta_ticks as f64 / clock_ticks as f64;
            let utilization = if elapsed_secs > 0.0 {
                cpu_seconds / elapsed_secs
            } else {
                0.0
            };
            cpu_gauge.set(utilization, &[]);
        }
        state.prev_cpu_ticks = Some(total_ticks);
        state.prev_instant = Some(now);
    }

    if let Some(rss_pages) = read_rss_pages() {
        let rss_bytes = rss_pages * page_size;
        mem_gauge.set(rss_bytes as f64, &[]);
    }
}

#[cfg(target_os = "linux")]
fn page_size_bytes() -> Option<u64> {
    // SAFETY: sysconf(_SC_PAGESIZE) is always safe to call.
    let val = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if val > 0 {
        Some(val as u64)
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
fn clock_ticks_per_sec() -> Option<u64> {
    // SAFETY: sysconf(_SC_CLK_TCK) is always safe to call.
    let val = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if val > 0 {
        Some(val as u64)
    } else {
        None
    }
}

/// Read utime (field 14) and stime (field 15) from `/proc/self/stat`.
#[cfg(target_os = "linux")]
fn read_cpu_ticks() -> Option<(u64, u64)> {
    let data = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Fields after the comm field (which is in parens and may contain spaces).
    let after_comm = data.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // After closing paren: field[0]=state, field[1]=ppid, ... field[11]=utime, field[12]=stime
    let utime = fields.get(11)?.parse::<u64>().ok()?;
    let stime = fields.get(12)?.parse::<u64>().ok()?;
    Some((utime, stime))
}

/// Read RSS (field 2) from `/proc/self/statm`.
#[cfg(target_os = "linux")]
fn read_rss_pages() -> Option<u64> {
    let data = std::fs::read_to_string("/proc/self/statm").ok()?;
    let rss = data.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    Some(rss)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_once_does_not_panic() {
        let mut state = UseMetricsState::default();
        poll_once(&mut state);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_cpu_ticks_returns_values() {
        let result = read_cpu_ticks();
        assert!(
            result.is_some(),
            "/proc/self/stat should be readable on Linux"
        );
        let (utime, stime) = result.unwrap();
        assert!(utime + stime > 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_rss_pages_returns_nonzero() {
        let result = read_rss_pages();
        assert!(
            result.is_some(),
            "/proc/self/statm should be readable on Linux"
        );
        assert!(
            result.unwrap() > 0,
            "RSS should be > 0 for a running process"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn page_size_is_reasonable() {
        let ps = page_size_bytes();
        assert!(ps.is_some(), "sysconf should succeed");
        assert!(ps.unwrap() >= 4096, "page size should be at least 4096");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn clock_ticks_is_reasonable() {
        let ct = clock_ticks_per_sec();
        assert!(ct.is_some(), "sysconf should succeed");
        assert!(ct.unwrap() >= 100, "clock ticks should be at least 100");
    }
}
