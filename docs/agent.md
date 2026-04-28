# `tak-agent` — headless TAK Protocol conformance agent

Cross-compiled native binary (defaults to `aarch64-linux-android`)
that drives the same scenarios as `tak-conformance` against a
**remote** TAK server. Intended deployment: `adb push` onto an
Android device sitting on the same network as a tak-rs (or upstream
Java) server, then run scenarios from there. The bytes the agent
sends and receives are the bytes ATAK Android would send and
receive on that same network — same kernel TCP stack, same MTU,
same NAT path — so a divergence here is a real divergence.

This is **Option A** from the agent design discussion: a headless
protocol exerciser, not a UIAutomator orchestration. It doesn't
prove ATAK's UI works; it proves the wire bytes work from a
device-side vantage point. UIAutomator-driven real-ATAK testing
is a separate scaffold.

## What it tests

Same registered scenarios as `tak-conformance`:

- **`pli_dispatch_byte_identity`** — Implemented. Two clients
  connect; A publishes a PLI; B receives a frame; bytes are
  asserted byte-identical to A's send.
- **`chat_xml_lossless`** — Stub. See `crates/tak-conformance/src/scenarios/chat_xml_lossless.rs`.
- **`replay_on_reconnect`** — Stub.

When new scenarios land in `tak-conformance::scenarios`, they
become available on the agent without code change — the agent's
registry just imports them.

## Three-step flow

### 1. Build for Android

```bash
# Prereqs: rustup target + cargo-ndk + Android NDK installed,
# ANDROID_NDK_HOME or ~/Android/Sdk/ndk/<version> set.
cargo install cargo-ndk
rustup target add aarch64-linux-android

./scripts/build-android-agent.sh
# -> target/aarch64-linux-android/release/tak-agent
```

If you don't have the NDK installed, the script will fail loudly
with `failed to find tool "aarch64-linux-android-clang"`. That's
the right error: the workspace `aws-lc-sys` (used by rustls) needs
NDK clang to compile.

### 2. Push to device + run

```bash
./scripts/run-agent-scenario.sh \
    target/aarch64-linux-android/release/tak-agent \
    all \
    <server-ip>:8088

# Output is JSON-lines, one record per scenario:
# {"scenario":"pli_dispatch_byte_identity","outcome":"PASS",...}
# {"scenario":"chat_xml_lossless","outcome":"SKIPPED",...}
```

If `<server-ip>` is reachable from the device — bridged emulator
network, same Wi-Fi, USB tethered, etc. — that's all that's
needed. The script also writes the run to
`./tak-agent-runs/<ts>-<scenario>.jsonl` for later analysis.

### 3. Reduce the JSON

```bash
# Show only failures from the last run:
jq 'select(.outcome=="FAIL")' tak-agent-runs/*.jsonl

# Pass rate across the suite:
jq -s '
  group_by(.outcome)
  | map({outcome: .[0].outcome, count: length})
' tak-agent-runs/<ts>-all.jsonl
```

## Running it on the host (no device)

The agent isn't Android-specific — it's a portable Rust binary that
talks TCP. Useful for sanity-checking before flashing an Android
device:

```bash
# Build native:
cargo build --release -p tak-agent

# Boot tak-rs in another terminal, then:
target/release/tak-agent all --target 127.0.0.1:8088
# PASS      pli_dispatch_byte_identity
# SKIPPED   chat_xml_lossless     not implemented; needs real ATAK chat capture
# SKIPPED   replay_on_reconnect   not implemented; replay-on-reconnect path missing
```

The host-mode run uses the same scenario code as the device-mode
run, so a passing host run is a precondition for trusting a
failing device run (rules out "the agent is broken, not the
server").

## How this fits with `tak-conformance`

| Tier | Backend | What it proves |
|---|---|---|
| 1 | `tak-conformance` in-process tests | Server doesn't regress on documented contracts. |
| 2 | `tak-agent` host-mode | Same contracts hold over a real TCP socket from a non-server process. |
| 3 | `tak-agent` device-mode (this) | Same contracts hold from a real Android device's network stack. |
| 4 | UIAutomator + real ATAK CIV | ATAK's UI actually works against the server. |

Tier 1 + 2 run in CI. Tier 3 runs nightly or on demand from a tethered device. Tier 4 (Option B from the design discussion) is a separate scaffold.

## Output format

Each `--json` invocation emits one object per scenario:

```json
{
  "scenario": "pli_dispatch_byte_identity",
  "description": "subscriber receives byte-identical frame fan-out for a published PLI",
  "target": "192.168.1.42:8088",
  "outcome": "PASS",
  "detail": null
}
```

`outcome` is one of `PASS` / `FAIL` / `SKIPPED`. `detail` is `null`
on PASS, otherwise a one-line operator-readable explanation
(scenario-specified). The agent exits non-zero iff any scenario
emitted `FAIL`; `SKIPPED` does not gate the run.

## Limitations (deferred to follow-ups)

- **mTLS:** the agent talks plain TCP. Wiring `rustls` against the
  cert chain produced by `scripts/gen-mtls-certs.sh` is a follow-up.
  For lab / bench networks this is fine; production networks will
  need it.
- **Connection-init XML preamble:** ATAK's full handshake includes
  a Protocol-v0 preamble + negotiation. The agent skips it (tak-rs
  does too on `:8088`); pointing the agent at the upstream Java
  server's `:8087` will need the preamble added.
- **pcap capture on device:** currently only stdout JSON-lines. A
  follow-up can run `tcpdump` alongside the agent and pull the
  capture back over `adb pull`.
- **Mission API REST flows:** scenario coverage today is
  firehose-only. REST scenarios need a different mock client
  (HTTP rather than streaming TCP) — same crate, different module.
