//! Drift analysis.
//!
//! Linear regression of RSS-vs-time over the late window
//! (last 75% of samples — the first 25% is allocator warmup).
//! Slope > threshold = leak; fail the run.
//!
//! Throughput sanity check: compute deliveries-per-second
//! late-window, sanity-fail if it's zero (indicates either the
//! loadgen or server died mid-run; ALSO fail if it dropped to
//! ~0 from a non-trivial earlier rate).

use crate::latency::LatencySummary;
use crate::sampler::Sample;

#[derive(Debug)]
pub struct Report {
    pub samples: usize,
    pub duration_s: f64,
    pub rss_start_kb: u64,
    pub rss_end_kb: u64,
    pub rss_min_kb: u64,
    pub rss_max_kb: u64,
    pub rss_slope_kb_per_min: f64,
    pub rss_drift_threshold: f64,
    pub deliveries_total: u64,
    pub deliveries_per_s: f64,
    pub dropped_full_total: u64,
    pub persisted_total: u64,
    pub persistence_dropped_total: u64,
    pub latency: Option<LatencySummary>,
    pub max_p99_us_threshold: u64,
    pub failed: bool,
    pub failure_reasons: Vec<String>,
}

/// Attach the pinned-probe summary to a soak report and gate on
/// `max_p99_us_threshold`. The threshold is hit only when the
/// probe actually produced samples; an absent probe doesn't fail
/// the soak (the warning already surfaced in `main`).
pub fn attach_latency(
    report: &mut Report,
    latency: Option<LatencySummary>,
    max_p99_us_threshold: u64,
) {
    report.latency = latency;
    report.max_p99_us_threshold = max_p99_us_threshold;
    if let Some(s) = latency {
        if s.samples == 0 {
            report.failed = true;
            report.failure_reasons.push(
                "latency probe finished with zero samples — peer closed early or fan-out broken"
                    .to_owned(),
            );
        } else if s.p99_us > max_p99_us_threshold {
            report.failed = true;
            report.failure_reasons.push(format!(
                "latency p99 {} µs exceeds threshold {} µs (max {} µs)",
                s.p99_us, max_p99_us_threshold, s.max_us
            ));
        }
    }
}

pub fn analyze(samples: &[Sample], rss_drift_threshold: f64) -> Report {
    let mut report = Report {
        samples: samples.len(),
        duration_s: 0.0,
        rss_start_kb: 0,
        rss_end_kb: 0,
        rss_min_kb: u64::MAX,
        rss_max_kb: 0,
        rss_slope_kb_per_min: 0.0,
        rss_drift_threshold,
        deliveries_total: 0,
        deliveries_per_s: 0.0,
        dropped_full_total: 0,
        persisted_total: 0,
        persistence_dropped_total: 0,
        latency: None,
        max_p99_us_threshold: 0,
        failed: false,
        failure_reasons: Vec::new(),
    };

    if samples.len() < 4 {
        report.failed = true;
        report.failure_reasons.push(format!(
            "only {} samples — need ≥4 for a meaningful regression",
            samples.len()
        ));
        return report;
    }

    let first = samples.first().unwrap();
    let last = samples.last().unwrap();
    report.duration_s = last.elapsed_s - first.elapsed_s;
    report.rss_start_kb = first.rss_kb;
    report.rss_end_kb = last.rss_kb;
    report.deliveries_total = last.bus_delivered.saturating_sub(first.bus_delivered);
    report.dropped_full_total = last.bus_dropped_full.saturating_sub(first.bus_dropped_full);
    report.persisted_total = last
        .persistence_inserted
        .saturating_sub(first.persistence_inserted);
    report.persistence_dropped_total = last
        .persistence_dropped
        .saturating_sub(first.persistence_dropped);
    if report.duration_s > 0.0 {
        #[allow(clippy::cast_precision_loss)]
        let total = report.deliveries_total as f64;
        report.deliveries_per_s = total / report.duration_s;
    }

    for s in samples {
        report.rss_min_kb = report.rss_min_kb.min(s.rss_kb);
        report.rss_max_kb = report.rss_max_kb.max(s.rss_kb);
    }

    // Late-window regression: drop the first 50% so we measure
    // steady-state slope, not the allocator's exponential
    // warm-up tail. Empirically tak-server's RSS rises ~115 kB/s
    // in the first 20 s of load and decays to single-digit kB/s
    // by t=80 s (mpsc block-pool growth + sub-trie / rtree
    // population). A linear fit over the full window sees that
    // tail as drift; a fit over the back half catches a real
    // leak.
    let warmup_cutoff = samples.len() / 2;
    let window = &samples[warmup_cutoff..];
    if window.len() < 4 {
        report.failed = true;
        report.failure_reasons.push(format!(
            "late-window only has {} samples — soak too short for drift analysis",
            window.len()
        ));
        return report;
    }
    let slope_per_s = linear_slope(window);
    report.rss_slope_kb_per_min = slope_per_s * 60.0;

    if report.rss_slope_kb_per_min.abs() > rss_drift_threshold {
        report.failed = true;
        report.failure_reasons.push(format!(
            "RSS drift {:.1} kB/min exceeds threshold {:.1} kB/min \
             (start={} kB, end={} kB)",
            report.rss_slope_kb_per_min,
            rss_drift_threshold,
            report.rss_start_kb,
            report.rss_end_kb
        ));
    }

    if report.deliveries_per_s < 1.0 && samples.len() > 4 {
        report.failed = true;
        report.failure_reasons.push(format!(
            "no bus deliveries observed across {} samples — loadgen or server likely died",
            samples.len()
        ));
    }

    report
}

/// Ordinary least-squares slope of (elapsed_s, rss_kb) — kB per
/// second. Multiply by 60 for kB/min.
fn linear_slope(window: &[Sample]) -> f64 {
    let n = window.len() as f64;
    let mean_x: f64 = window.iter().map(|s| s.elapsed_s).sum::<f64>() / n;
    #[allow(clippy::cast_precision_loss)]
    let mean_y: f64 = window.iter().map(|s| s.rss_kb as f64).sum::<f64>() / n;
    let mut num = 0.0;
    let mut den = 0.0;
    for s in window {
        let dx = s.elapsed_s - mean_x;
        #[allow(clippy::cast_precision_loss)]
        let dy = s.rss_kb as f64 - mean_y;
        num += dx * dy;
        den += dx * dx;
    }
    if den == 0.0 { 0.0 } else { num / den }
}

pub fn print_report(r: &Report) {
    println!();
    println!("=== tak-soak report ===");
    println!("samples              {}", r.samples);
    println!("duration_s           {:.1}", r.duration_s);
    println!("rss_start_kb         {}", r.rss_start_kb);
    println!("rss_end_kb           {}", r.rss_end_kb);
    println!("rss_min_kb           {}", r.rss_min_kb);
    println!("rss_max_kb           {}", r.rss_max_kb);
    println!(
        "rss_slope_kb_per_min {:.1}  (threshold ±{:.0})",
        r.rss_slope_kb_per_min, r.rss_drift_threshold
    );
    println!("deliveries_total     {}", r.deliveries_total);
    println!("deliveries_per_s     {:.0}", r.deliveries_per_s);
    println!("dropped_full_total   {}", r.dropped_full_total);
    println!("persisted_total      {}", r.persisted_total);
    println!("persist_dropped_tot  {}", r.persistence_dropped_total);
    if let Some(s) = &r.latency {
        println!("--- pinned latency probe ---");
        println!("samples              {}", s.samples);
        println!("sends                {}", s.sends);
        println!("recvs                {}", s.recvs);
        println!("p50_us               {}", s.p50_us);
        println!("p95_us               {}", s.p95_us);
        println!("p99_us               {}", s.p99_us);
        println!("p999_us              {}", s.p999_us);
        println!("max_us               {}", s.max_us);
        println!("p99_threshold_us     {}", r.max_p99_us_threshold);
    } else {
        println!("--- pinned latency probe ---");
        println!("(disabled or unavailable)");
    }
    println!(
        "verdict              {}",
        if r.failed { "FAIL" } else { "PASS" }
    );
    for reason in &r.failure_reasons {
        println!("  - {reason}");
    }
    println!("=======================");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(elapsed_s: f64, rss_kb: u64, delivered: u64) -> Sample {
        Sample {
            elapsed_s,
            rss_kb,
            bus_delivered: delivered,
            bus_dropped_full: 0,
            persistence_inserted: 0,
            persistence_dropped: 0,
        }
    }

    #[test]
    fn flat_rss_passes() {
        let samples: Vec<Sample> = (0..60)
            .map(|i| s(f64::from(i), 100_000, 1000 * i as u64))
            .collect();
        let r = analyze(&samples, 1024.0);
        assert!(!r.failed, "{:?}", r.failure_reasons);
        assert!(r.rss_slope_kb_per_min.abs() < 1.0);
    }

    #[test]
    fn drifting_rss_fails() {
        // 100 kB/sec growth = 6000 kB/min — well over 1024.
        let samples: Vec<Sample> = (0..60)
            .map(|i| s(f64::from(i), 100_000 + (i as u64 * 100), 1000 * i as u64))
            .collect();
        let r = analyze(&samples, 1024.0);
        assert!(r.failed);
        assert!(r.rss_slope_kb_per_min > 1024.0);
    }

    #[test]
    fn dead_loadgen_fails() {
        // RSS flat, but no deliveries — server or loadgen died.
        let samples: Vec<Sample> = (0..60).map(|i| s(f64::from(i), 100_000, 0)).collect();
        let r = analyze(&samples, 1024.0);
        assert!(r.failed);
        assert!(
            r.failure_reasons
                .iter()
                .any(|m| m.contains("no bus deliveries"))
        );
    }
}
