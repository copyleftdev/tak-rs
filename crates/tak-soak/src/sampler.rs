//! Per-tick sampler.
//!
//! Reads the tak-server's RSS from `/proc/<pid>/status` and the
//! prom metrics endpoint, every `interval`. Push samples to a
//! Vec; analyze::analyze consumes it after the run.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::metrics::parse_text;
use crate::server::read_url_text;

#[derive(Debug, Clone)]
pub struct Sample {
    /// Seconds since soak start. Float so the regression doesn't
    /// have to discretize.
    pub elapsed_s: f64,
    pub rss_kb: u64,
    /// Cumulative counter snapshots; deltas are computed at
    /// analyze time.
    pub bus_delivered: u64,
    pub bus_dropped_full: u64,
    pub persistence_inserted: u64,
    pub persistence_dropped: u64,
}

pub async fn run(
    pid: u32,
    metrics_url: &str,
    duration: Duration,
    interval: Duration,
) -> Vec<Sample> {
    let mut out = Vec::with_capacity((duration.as_secs() / interval.as_secs().max(1)) as usize);
    let started = Instant::now();
    let mut next_tick = started + interval;

    while started.elapsed() < duration {
        let now = Instant::now();
        if now < next_tick {
            tokio::time::sleep(next_tick - now).await;
        }
        next_tick += interval;

        let elapsed_s = started.elapsed().as_secs_f64();
        let rss_kb = read_rss_kb(pid).unwrap_or(0);
        let metrics_body = read_url_text(metrics_url, Duration::from_secs(2))
            .await
            .unwrap_or_default();
        let m = parse_text(&metrics_body);
        out.push(Sample {
            elapsed_s,
            rss_kb,
            bus_delivered: m.get("tak_bus_delivered").copied().unwrap_or(0.0).max(0.0) as u64,
            bus_dropped_full: m
                .get("tak_bus_dropped_full")
                .copied()
                .unwrap_or(0.0)
                .max(0.0) as u64,
            // tak-store emits these via metrics::counter!; the
            // exporter mangles the dot to underscore.
            persistence_inserted: m
                .get("tak_persistence_inserted")
                .copied()
                .unwrap_or(0.0)
                .max(0.0) as u64,
            persistence_dropped: m
                .get("tak_persistence_dropped")
                .copied()
                .unwrap_or(0.0)
                .max(0.0) as u64,
        });
    }
    out
}

/// Parse VmRSS from `/proc/<pid>/status`. Returns RSS in kB.
fn read_rss_kb(pid: u32) -> std::io::Result<u64> {
    let body = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            // VmRSS:    12345 kB
            for tok in rest.split_whitespace() {
                if let Ok(v) = tok.parse::<u64>() {
                    return Ok(v);
                }
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "VmRSS not found",
    ))
}

pub fn write_csv(path: &Path, samples: &[Sample]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    writeln!(
        f,
        "elapsed_s,rss_kb,bus_delivered,bus_dropped_full,persistence_inserted,persistence_dropped"
    )?;
    for s in samples {
        writeln!(
            f,
            "{:.3},{},{},{},{},{}",
            s.elapsed_s,
            s.rss_kb,
            s.bus_delivered,
            s.bus_dropped_full,
            s.persistence_inserted,
            s.persistence_dropped
        )?;
    }
    Ok(())
}
