---
description: Run the firehose criterion bench, compare against the saved baseline, and report the delta. Optionally promote `current` to `main` baseline with `--save`.
argument-hint: [bench-name] [--save]
allowed-tools: Read, Write, Bash, Glob, Agent
---

Invoke the `bench-baseline` agent to run criterion benches and produce a delta report.

Default bench is `firehose` (the main hot-path bench in `crates/tak-bus/benches/firehose.rs`). Other named benches: `codec`, `framing`. `$ARGUMENTS` may be empty (defaults to `firehose`), or a bench name, optionally followed by `--save` to promote `current` → `main` baseline after the user confirms the change is intentional.

The agent will:
1. Verify clean git state and warn if dirty.
2. Check CPU governor (Linux) and pin to a performance core via `taskset` where available.
3. Run `cargo bench --bench <name> -- --save-baseline current`.
4. Compute throughput + latency delta vs `main` baseline.
5. Write a JSON entry to `bench/history/<UTC-timestamp>-<name>.json` for long-term trend analysis.
6. Report verdict: IMPROVED / WITHIN-NOISE / REGRESSED.
7. If `--save` and verdict is IMPROVED or WITHIN-NOISE, promote `current` → `main` after explicit user confirmation.

Pre-flight: confirm `cargo bench --list` runs without error in the workspace. If the workspace doesn't exist yet (no `Cargo.toml` at root), tell the user to run `/scaffold` first.
