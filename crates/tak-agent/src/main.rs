//! `tak-agent` — headless TAK Protocol conformance agent.
//!
//! Cross-compiles to `aarch64-linux-android` (and any other target
//! the workspace builds for). Runs the same scenarios as
//! `tak-conformance`'s in-process suite, but against a *remote*
//! firehose. The intended deployment is `adb push` onto an Android
//! device sitting on the same network as a tak-rs (or upstream
//! Java) server.
//!
//! ## Why a separate binary
//!
//! `tak-conformance` boots a tak-server in-process via
//! testcontainers — that's CI's job. The agent is for the orthogonal
//! question: "given a server somewhere on the network, does it
//! satisfy the wire contract from this client's vantage point?"
//! Same scenarios, different system-under-test.
//!
//! ## Output
//!
//! By default emits human-readable text to stdout. With `--json`,
//! emits one structured outcome per line so an orchestrator
//! (`adb shell tak-agent ... | jq`) can reduce the result.
//!
//! ## Binary D1 exemption
//!
//! Same as `tak-server` and `taktool`: this is a process boundary
//! that owns argv parsing and bootstrap logging, so the lib-side
//! `unwrap`/`print*` bans are off here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr
)]

use std::net::SocketAddr;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;
use tak_conformance::scenarios::chat_xml_lossless::ChatXmlLossless;
use tak_conformance::scenarios::pli_dispatch_byte_identity::PliDispatchByteIdentity;
use tak_conformance::scenarios::replay_on_reconnect::ReplayOnReconnect;
use tak_conformance::{Outcome, Scenario};

#[derive(Parser, Debug)]
#[command(
    name = "tak-agent",
    version,
    about = "headless TAK Protocol conformance agent",
    long_about = None,
)]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// List the registered scenarios and exit.
    List,
    /// Run one named scenario.
    Run(RunArgs),
    /// Run every registered scenario in sequence; emit a report.
    All(RunAllArgs),
}

#[derive(Parser, Debug)]
struct RunArgs {
    /// Scenario name. `tak-agent list` shows the available set.
    name: String,
    /// Firehose to dial — `host:port` of the tak-rs (or Java) server.
    #[arg(long, env = "TAK_AGENT_TARGET", default_value = "127.0.0.1:8088")]
    target: SocketAddr,
    /// Emit one JSON object to stdout instead of human-readable text.
    /// Suitable for piping into `jq` from an orchestrator.
    #[arg(long, env = "TAK_AGENT_JSON", default_value_t = false)]
    json: bool,
}

#[derive(Parser, Debug)]
struct RunAllArgs {
    /// Firehose to dial.
    #[arg(long, env = "TAK_AGENT_TARGET", default_value = "127.0.0.1:8088")]
    target: SocketAddr,
    /// JSON-lines output (one object per scenario).
    #[arg(long, env = "TAK_AGENT_JSON", default_value_t = false)]
    json: bool,
}

/// Wire-shape JSON record an orchestrator can `jq` over.
#[derive(Serialize)]
struct Record<'a> {
    scenario: &'a str,
    description: &'a str,
    target: String,
    outcome: &'a str,
    detail: Option<&'a str>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Logs go to stderr so stdout stays clean for `--json`
    // consumers piping through jq.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,tak_agent=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    match args.cmd {
        Cmd::List => {
            for sc in registered_scenarios() {
                println!("{:32}  {}", sc.name(), sc.description());
            }
            Ok(())
        }
        Cmd::Run(a) => run_one(a).await,
        Cmd::All(a) => run_all(a).await,
    }
}

async fn run_one(a: RunArgs) -> Result<()> {
    let sc = registered_scenarios()
        .into_iter()
        .find(|s| s.name() == a.name)
        .with_context(|| format!("unknown scenario {:?}; try `tak-agent list`", a.name))?;
    let outcome = sc.run(a.target).await;
    emit(sc.as_ref(), a.target, &outcome, a.json);
    if matches!(outcome, Outcome::Fail(_)) {
        std::process::exit(1);
    }
    Ok(())
}

async fn run_all(a: RunAllArgs) -> Result<()> {
    let mut any_fail = false;
    for sc in registered_scenarios() {
        let outcome = sc.run(a.target).await;
        if matches!(outcome, Outcome::Fail(_)) {
            any_fail = true;
        }
        emit(sc.as_ref(), a.target, &outcome, a.json);
    }
    if any_fail {
        std::process::exit(1);
    }
    Ok(())
}

fn emit(sc: &dyn Scenario, target: SocketAddr, outcome: &Outcome, json: bool) {
    let (label, detail) = match outcome {
        Outcome::Pass => ("PASS", None),
        Outcome::Fail(why) => ("FAIL", Some(why.as_str())),
        Outcome::Skipped(why) => ("SKIPPED", Some(why.as_str())),
    };
    if json {
        let rec = Record {
            scenario: sc.name(),
            description: sc.description(),
            target: target.to_string(),
            outcome: label,
            detail,
        };
        // Best-effort serialize; failure is highly unlikely with
        // our owned scalar fields, but if it ever does we print
        // an explicit fallback so an orchestrator doesn't see
        // empty stdout.
        match serde_json::to_string(&rec) {
            Ok(line) => println!("{line}"),
            Err(e) => println!(
                "{{\"scenario\":\"{}\",\"outcome\":\"FAIL\",\"detail\":\"json serialize: {e}\"}}",
                sc.name()
            ),
        }
    } else {
        match detail {
            Some(why) => println!("{label:<8}  {:<40}  {why}", sc.name()),
            None => println!("{label:<8}  {:<40}", sc.name()),
        }
    }
}

fn registered_scenarios() -> Vec<Box<dyn Scenario>> {
    vec![
        Box::new(PliDispatchByteIdentity),
        Box::new(ChatXmlLossless),
        Box::new(ReplayOnReconnect),
    ]
}
