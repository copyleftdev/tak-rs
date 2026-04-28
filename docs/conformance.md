# ATAK conformance — runbook

The `tak-conformance` crate locks in the wire-protocol contract that
`tak-rs` must satisfy to be a drop-in for the upstream Java server.
A passing run of the synthetic suite is **necessary but not
sufficient**: it doesn't prove ATAK Android works, only that the
server doesn't regress on the contracts we've explicitly written
down.

This runbook is what you do when you have a real ATAK device and
want to find out what we got wrong.

## Levels of confidence

| Level | What it proves | How to run |
|---|---|---|
| **Synthetic suite** | Server doesn't regress on documented contracts. | `cargo nextest run -p tak-conformance --run-ignored=ignored-only` |
| **Side-by-side diff vs. Java** | Server bytes-match the Java reference for the same input. | (TODO: `bench/java/` harness wires it up) |
| **Live ATAK device** | Server actually works for an ATAK user. | This document. |

Treat every divergence found at a higher level as a P0 — codify it
as a new scenario in `crates/tak-conformance/src/scenarios/` so the
gap can never silently re-open.

## What the synthetic suite covers today

Run `cargo nextest run -p tak-conformance --run-ignored=ignored-only --no-capture`
to see the live report. As of this writing:

- ✅ **`pli_dispatch_byte_identity`** — A publishes a PLI frame; B
  (also connected) receives byte-for-byte the same bytes back.
  This is the most fundamental contract; if it ever fails, ATAK
  icons will desync silently.
- ⏭️ **`chat_xml_lossless`** — STUB. Needs a real ATAK chat
  capture to exercise namespace + CDATA edge cases. The codec
  invariant says `Detail.xmlDetail` is preserved as a borrowed
  `&str`; this scenario will pin that down through the firehose
  once a real fixture exists.
- ⏭️ **`replay_on_reconnect`** — STUB. The Java server replays
  the last N hours of `cot_router` events to a reconnecting
  client; `tak-rs` does not. Reconnecting ATAK clients will see
  ghosts until peers re-emit. This is **Tier-1 punch-list item
  #2** in the drop-in readiness assessment.

When new gaps land, add a stub scenario *before* fixing them so
the contract is visible in the report before the implementation.

## Pointing a real ATAK device at `tak-rs`

### 1. Generate mTLS certs ATAK trusts

ATAK ships with a pinned set of root CAs. To use a self-signed
chain, ATAK has to install a CA `.p12` and a per-user client
`.p12`. The repo includes `scripts/gen-mtls-certs.sh` for the
bench / dev path:

```bash
./scripts/gen-mtls-certs.sh /tmp/tak-certs <server-ip-or-hostname>
```

Outputs:
- `ca.pem` — the CA cert ATAK installs as a trusted root.
- `ca.key` — keep secret on the server side.
- `server.pem` / `server.key` — what tak-rs presents.
- `client-VIPER01.p12` — what ATAK installs for client auth.
  Default password: `atakatak`.

For production: use a real CA, never these certs.

### 2. Boot tak-rs with mTLS on 8089

```bash
target/release/tak-server \
    --database-url postgres://tak:takatak@127.0.0.1/tak \
    --listen-cot 0.0.0.0:8088 \
    --listen-api 0.0.0.0:8080 \
    --listen-metrics 0.0.0.0:9091
```

> **Gap:** mTLS termination on 8089 is not yet wired into the
> firehose binary. The `tak-net` crate has the rustls plumbing
> (`crates/tak-net/src/tls.rs`); wiring it into `firehose::run`
> on a second listener address is a one-commit change but
> hasn't landed. For the runbook, point ATAK at port 8088 over
> plain TCP — fine for bench / lab, never production.

### 3. Configure ATAK

On the device:

1. **Settings → Network Connection Preferences → Manage Server
   Connections → Add → TAK Server**.
2. **Description**: any name.
3. **Address**: `<server-host>:8088`.
4. **Protocol**: `TCP` (not SSL, until the mTLS gap lands).
5. **Use Auth**: off.

After save, ATAK should connect within a few seconds. The
firehose log will show:

```json
{"message":"firehose: accepted","conn":0,"peer":"<atak-ip>:..."}
```

### 4. What to watch

While ATAK is connected, scrape `:9091/metrics`:

```
tak_bus_delivered                 # how many fan-outs landed
tak_bus_dropped_full              # H5 mpsc-full drops (per-sub overload)
tak_bus_dropped_closed            # peer dropped before delivery
tak_bus_filtered_groups           # group-mask rejections
tak.persistence.inserted          # rows landed in cot_router
tak.persistence.dropped           # persistence side-channel overruns
```

The 10 s `subscription dropwatch tick` log line shows top-N slow
subscribers. If ATAK is alone on the server, every one of these
should be near-zero.

### 5. Things to deliberately try

| Action | Expected | Watch |
|---|---|---|
| ATAK sends a PLI | New row in `cot_router`, `tak_bus_delivered` ticks. | `psql -c "select uid, callsign, time_ms from cot_router order by time_ms desc limit 5"` |
| Two ATAKs both connected, A sends a chat to B | B receives the chat (visible in B's UI). | tak-rs logs show two `firehose: accepted`; B's writer drains. |
| ATAK sends a drawing or geofence | Row persists, `Detail.xmlDetail` survives. | Compare the persisted `detail` text to the original wire bytes — should be byte-identical. |
| Disconnect ATAK, reconnect 1 minute later | **Java**: ATAK sees recent peer PLIs without waiting. **tak-rs**: nothing until peers re-emit. | This is the `replay_on_reconnect` gap. |
| Mission API: create a mission via `/missions` | Mission appears in ATAK's mission list. | `curl -s :8080/missions` |
| Slow subscriber — connect with `nc` and don't read | `subscription dropwatch tick` shows the `nc` socket at high drop %. | Logs name the slow sub by `gen=`. |

### 6. When something breaks

1. Capture the last ~30 s of tracing output (`journalctl -u
   tak-server`, or whatever your runner is).
2. Capture a tcpdump of the ATAK ↔ tak-rs traffic:
   ```bash
   tcpdump -i any -w atak-divergence.pcap port 8088
   ```
3. File a scenario in `crates/tak-conformance/src/scenarios/`
   that reproduces the divergence (use the captured frames as
   fixtures). The synthetic suite then guards against it
   regressing.
4. Fix the server.

### 7. Known gaps (will surface during real-ATAK testing)

| Symptom | Likely cause |
|---|---|
| ATAK reconnects, peer icons missing for ~minutes | `replay_on_reconnect` not implemented. |
| ATAK refuses to connect over TLS | `--listen-cot` is plain TCP only; TLS on 8089 not wired. |
| Group filtering not applied (everyone sees everyone) | `firehose::handle_connection` uses `ALL_GROUPS`; cert→group mapping not implemented. |
| Mission packages don't appear | Mission API beyond CRUD is not implemented. |
| ATAK certificate enrollment flow fails | Not implemented; ATAK has to be pre-provisioned with a `.p12`. |

The conformance suite already names these as stub scenarios so
they can never silently re-open once fixed.

## Adding a new scenario from a real-ATAK divergence

```rust
// crates/tak-conformance/src/scenarios/<your_name>.rs
#[derive(Debug, Default)]
pub struct YourScenario;

impl Scenario for YourScenario {
    fn name(&self) -> &'static str { "your_name" }
    fn description(&self) -> &'static str { "what it asserts in one line" }

    fn run<'a>(&'a self, server: &'a TestServer)
        -> Pin<Box<dyn Future<Output = Outcome> + Send + 'a>>
    {
        Box::pin(async move {
            // 1. Bake the captured frame into bytes.
            // 2. Connect mock clients.
            // 3. Drive the exchange.
            // 4. Assert. Return Outcome::Pass / Fail / Skipped.
        })
    }
}
```

Then add it to `tests/run_scenarios.rs`. PR review checklist:
- The scenario name is searchable and survives a future read by
  someone who has not seen this commit.
- The failure message in `Outcome::Fail` names the exact
  divergence (offset, expected vs got), not just "mismatch."
- A captured fixture lives next to the scenario, with provenance
  (which ATAK build, which firmware, what the operator was doing
  when they captured it).
