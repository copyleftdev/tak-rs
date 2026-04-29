//! Pin a `taktool latency` probe alongside the soak's loadgen.
//!
//! The probe gives us a single number per soak run for end-to-end
//! dispatch RTT, captured under exactly the load the soak is
//! driving. Without it, the soak only answers "did RSS leak"; with
//! it, the soak also answers "did p99 latency drift". A drop in
//! delivery rate without an RSS leak — the kind of degradation a
//! pure RSS check misses — shows up as a p99 cliff here.
//!
//! Mechanics:
//! - Spawn `taktool latency --json --target ... --rate ...
//!   --duration <soak_duration>`.
//! - stdout is captured to a tempfile so we can parse the single
//!   JSON line at end-of-run; stderr goes to a side log for
//!   debugging.
//! - We don't want the probe to itself perturb the latency it's
//!   measuring, so the probe rate stays low (default 20 Hz). At
//!   that rate the per-sub mpsc capacity is never close to full
//!   even under heavy fan-out.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::{Child, Command};

/// Final summary parsed from `taktool latency --json`.
#[derive(Debug, Clone, Copy, Default)]
pub struct LatencySummary {
    pub samples: u64,
    pub sends: u64,
    pub recvs: u64,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub p999_us: u64,
    pub max_us: u64,
}

/// Spawn the probe. Returns the child + the path the JSON line will
/// land in (parse it with [`parse_summary`] after the probe exits).
pub fn spawn(
    taktool_bin: &Path,
    target: &str,
    rate: u32,
    duration_secs: u64,
) -> Result<(Child, PathBuf)> {
    let stdout_path =
        std::env::temp_dir().join(format!("tak-soak-latency-{}.log", std::process::id()));
    let stderr_path =
        std::env::temp_dir().join(format!("tak-soak-latency-{}.err.log", std::process::id()));

    let stdout = std::fs::File::create(&stdout_path)
        .with_context(|| format!("create {}", stdout_path.display()))?;
    let stderr = std::fs::File::create(&stderr_path)
        .with_context(|| format!("create {}", stderr_path.display()))?;

    let child = Command::new(taktool_bin)
        .args([
            "latency",
            "--target",
            target,
            "--rate",
            &rate.to_string(),
            "--duration",
            // Probe matches loadgen tail: same head-room we apply
            // to loadgen so the probe doesn't tear down before the
            // soak's final metrics tick.
            &(duration_secs + 5).to_string(),
            "--json",
        ])
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("spawn taktool latency")?;

    Ok((child, stdout_path))
}

/// Parse the `--json` summary line from a latency-probe stdout log.
///
/// Single-line JSON of the shape produced by `taktool latency
/// --json`. We scan the whole file rather than relying on line
/// position (the probe also prints a human-readable block) — the
/// JSON is the line that starts with `{`.
///
/// # Errors
///
/// - File can't be read.
/// - No JSON line found (probe likely crashed before final
///   summary).
pub fn parse_summary(path: &Path) -> Result<LatencySummary> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let line = raw
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .ok_or_else(|| anyhow::anyhow!("no JSON summary line in {}", path.display()))?;

    Ok(LatencySummary {
        samples: extract_u64(line, "\"samples\":"),
        sends: extract_u64(line, "\"sends\":"),
        recvs: extract_u64(line, "\"recvs\":"),
        p50_us: extract_u64(line, "\"p50_us\":"),
        p95_us: extract_u64(line, "\"p95_us\":"),
        p99_us: extract_u64(line, "\"p99_us\":"),
        p999_us: extract_u64(line, "\"p999_us\":"),
        max_us: extract_u64(line, "\"max_us\":"),
    })
}

/// Tiny single-key extractor — taktool's JSON output is flat
/// numeric so a real serde dependency isn't worth the build cost
/// for one line. Returns 0 on miss; the caller's report
/// surfaces the resulting zeros loudly.
fn extract_u64(line: &str, key: &str) -> u64 {
    let Some(start) = line.find(key) else {
        return 0;
    };
    let after = &line[start + key.len()..];
    let end = after
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after.len());
    after[..end].parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extracts_all_fields() {
        let line = r#"{"target":"127.0.0.1:18088","rate":20,"duration":300,"sends":6000,"recvs":6000,"samples":6000,"p50_us":89,"p95_us":231,"p99_us":376,"p999_us":1387,"max_us":1387}"#;
        let s = LatencySummary {
            samples: extract_u64(line, "\"samples\":"),
            sends: extract_u64(line, "\"sends\":"),
            recvs: extract_u64(line, "\"recvs\":"),
            p50_us: extract_u64(line, "\"p50_us\":"),
            p95_us: extract_u64(line, "\"p95_us\":"),
            p99_us: extract_u64(line, "\"p99_us\":"),
            p999_us: extract_u64(line, "\"p999_us\":"),
            max_us: extract_u64(line, "\"max_us\":"),
        };
        assert_eq!(s.samples, 6000);
        assert_eq!(s.p50_us, 89);
        assert_eq!(s.p99_us, 376);
        assert_eq!(s.p999_us, 1387);
        assert_eq!(s.max_us, 1387);
    }

    #[test]
    fn parse_summary_finds_line_after_human_block() {
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("latency-test-{}.log", std::process::id()));
        let body = "=== latency probe ===\nrate            20 Hz\np99  (us)       376\nmax  (us)       1387\n=====================\n{\"target\":\"x\",\"rate\":20,\"duration\":10,\"sends\":200,\"recvs\":200,\"samples\":200,\"p50_us\":86,\"p95_us\":281,\"p99_us\":456,\"p999_us\":565,\"max_us\":565}\n";
        std::fs::write(&tmp, body).unwrap();
        let s = parse_summary(&tmp).unwrap();
        assert_eq!(s.samples, 200);
        assert_eq!(s.p99_us, 456);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn missing_field_returns_zero() {
        let line = r#"{"samples":100}"#;
        assert_eq!(extract_u64(line, "\"missing\":"), 0);
        assert_eq!(extract_u64(line, "\"samples\":"), 100);
    }
}
