//! Repro artifact: the smallest thing that re-runs a VOPR failure.
//!
//! Three fields:
//! - `seed`  — the ChaCha8 seed that drove the original op stream.
//! - `ops`   — the original `--ops` count.
//! - `skip`  — sorted list of op-stream indices to bypass during
//!   replay. Empty = the original failure trace; populated by
//!   `--minimize` as the harness shrinks the trace.
//!
//! Same `(seed, ops, skip)` always produces a bit-identical run.
//! That's the load-bearing property: replay never changes the
//! verdict; minimize only ever moves the verdict from FAIL → FAIL
//! (kept skip) or FAIL → PASS (rejected skip, restored).
//!
//! JSON-serialized so the artifact is hand-readable; the `skip`
//! list grows during minimization but stays ≤ original op count.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Repro {
    pub seed: u64,
    pub ops: u64,
    /// Op indices to bypass during replay. Stored as Vec so it
    /// JSON-pretty-prints in a sane order; converted to HashSet
    /// at apply time.
    #[serde(default)]
    pub skip: Vec<u64>,
}

impl Repro {
    pub fn skip_set(&self) -> HashSet<u64> {
        self.skip.iter().copied().collect()
    }

    /// Default failure dump path: `failure-<seed-hex>.json` in the
    /// CWD. Operators can re-run `tak-bus-vopr --replay` against
    /// this path with no other context.
    pub fn default_failure_path(seed: u64) -> std::path::PathBuf {
        std::path::PathBuf::from(format!("failure-{seed:#018x}.json"))
    }

    pub fn default_min_path(input: &Path) -> std::path::PathBuf {
        let mut out = input.to_path_buf();
        let stem = out
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("failure");
        let ext = out.extension().and_then(|s| s.to_str()).unwrap_or("json");
        out.set_file_name(format!("{stem}.min.{ext}"));
        out
    }

    pub fn write_json(&self, path: &Path) -> std::io::Result<()> {
        let mut sorted = self.skip.clone();
        sorted.sort_unstable();
        let pretty = Self {
            seed: self.seed,
            ops: self.ops,
            skip: sorted,
        };
        let json = serde_json::to_string_pretty(&pretty)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn read_json(path: &Path) -> std::io::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        serde_json::from_str(&raw)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}
