//! xtask — project automation for tak-rs.
//!
//! Conventional Rust pattern: a workspace-local binary crate that
//! provides project-specific verbs accessed via `cargo xt <verb>`.
//! See <https://github.com/matklad/cargo-xtask>.
//!
//! # Why a Rust binary instead of bash?
//!
//! Most of our automation already lives in `scripts/*.sh`. xtask is
//! reserved for verbs that need to walk the workspace (parse
//! `Cargo.toml`, diff vendored proto trees, generate code) where bash
//! gets fragile fast.
//!
//! # Verbs
//!
//! - `proto-diff` — compare `.proto` files in `crates/tak-proto/proto/`
//!   against the upstream Java tree under `.scratch/takserver-java/`.
//!   Reports which files are present in one tree but not the other,
//!   plus byte-equality for the intersection. Used before invoking
//!   `/proto-sync` to confirm what's about to change.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr
)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "xtask", about = "tak-rs project automation", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Diff vendored .proto files against the upstream Java tree.
    ProtoDiff(ProtoDiffArgs),
}

#[derive(clap::Args, Debug)]
struct ProtoDiffArgs {
    /// Path to the upstream Java tree (defaults to .scratch/takserver-java).
    #[arg(long)]
    upstream: Option<PathBuf>,

    /// Path to vendored proto dir (defaults to crates/tak-proto/proto).
    #[arg(long)]
    vendored: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::ProtoDiff(args) => proto_diff(&args),
    }
}

fn workspace_root() -> Result<PathBuf> {
    // xtask runs via `cargo run -p xtask`, so the working directory is
    // wherever the user invoked it (workspace root in practice). We
    // anchor relative paths from CARGO_MANIFEST_DIR/.. to be robust.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .context("workspace root: failed to ascend two levels from CARGO_MANIFEST_DIR")
}

fn proto_diff(args: &ProtoDiffArgs) -> Result<()> {
    let root = workspace_root()?;
    let vendored = args
        .vendored
        .clone()
        .unwrap_or_else(|| root.join("crates/tak-proto/proto"));
    let upstream = args
        .upstream
        .clone()
        .unwrap_or_else(|| root.join(".scratch/takserver-java"));

    if !vendored.is_dir() {
        bail!("vendored proto dir not found: {}", vendored.display());
    }
    if !upstream.is_dir() {
        bail!(
            "upstream tree not found: {} (run `git clone --depth=1 https://github.com/TAK-Product-Center/Server .scratch/takserver-java` first)",
            upstream.display(),
        );
    }

    let vendored_files = collect_protos(&vendored)?;
    let upstream_files = walk_protos(&upstream)?;

    println!(
        "vendored : {} ({} files)",
        vendored.display(),
        vendored_files.len()
    );
    println!(
        "upstream : {} ({} files)",
        upstream.display(),
        upstream_files.len()
    );
    println!();

    // We compare by basename only — the upstream tree scatters proto
    // files across multiple subprojects, but our vendored dir is
    // flattened.
    let mut vendored_by_name = std::collections::BTreeMap::new();
    for p in &vendored_files {
        if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
            vendored_by_name.insert(name.to_owned(), p.clone());
        }
    }

    let mut upstream_by_name = std::collections::BTreeMap::new();
    for p in &upstream_files {
        if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
            // If the same basename appears in multiple upstream
            // subprojects, the alphabetically-last path wins —
            // deterministic, and good enough for an advisory diff.
            upstream_by_name.insert(name.to_owned(), p.clone());
        }
    }

    let only_vendored: Vec<_> = vendored_by_name
        .keys()
        .filter(|k| !upstream_by_name.contains_key(*k))
        .collect();
    let only_upstream: Vec<_> = upstream_by_name
        .keys()
        .filter(|k| !vendored_by_name.contains_key(*k))
        .collect();

    if !only_vendored.is_empty() {
        println!("only in vendored ({}):", only_vendored.len());
        for n in &only_vendored {
            println!("  + {n}");
        }
        println!();
    }

    if !only_upstream.is_empty() {
        println!("only in upstream ({}):", only_upstream.len());
        for n in &only_upstream {
            println!("  + {n}");
        }
        println!();
    }

    // Byte-equality check across the intersection.
    let mut diffs = 0;
    let mut equal = 0;
    for (name, vp) in &vendored_by_name {
        if let Some(up) = upstream_by_name.get(name) {
            let v = std::fs::read(vp).with_context(|| format!("read {}", vp.display()))?;
            let u = std::fs::read(up).with_context(|| format!("read {}", up.display()))?;
            if v == u {
                equal += 1;
            } else {
                diffs += 1;
                println!("DIFF {name}");
                println!("    vendored: {} ({} bytes)", vp.display(), v.len());
                println!("    upstream: {} ({} bytes)", up.display(), u.len());
            }
        }
    }
    println!();
    println!("identical: {equal}");
    println!("differing: {diffs}");

    if diffs > 0 || !only_upstream.is_empty() {
        println!();
        println!("→ run `/proto-sync` to refresh vendored protos from upstream.");
    }
    Ok(())
}

fn collect_protos(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|e| e == "proto") {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn walk_protos(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk_recursive(dir, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            // Skip the usual noise — git internals, build dirs, etc.
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && matches!(
                    name,
                    ".git" | "build" | "target" | "node_modules" | ".gradle"
                )
            {
                continue;
            }
            walk_recursive(&path, out)?;
        } else if path.is_file() && path.extension().is_some_and(|e| e == "proto") {
            out.push(path);
        }
    }
    Ok(())
}
