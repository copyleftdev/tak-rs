---
description: Replay a captured TAK pcap through tak-server and verify the outputs match expected. Useful for regression testing with real ATAK/iTAK traffic.
argument-hint: <path-to-pcap> [--verify-against=<json>]
allowed-tools: Read, Write, Bash, Glob
---

Replay a real-world TAK packet capture through a local `tak-server` instance to verify behavior on representative traffic.

`$ARGUMENTS` must include the pcap path. Optional `--verify-against=<json>` points to an expected-output JSON file (a list of CoT events the server should emit downstream after replay).

Process:

1. Confirm the pcap exists and is readable. Use `tcpdump -r <path> -c 5` (or `tshark`) to peek the first 5 packets; report the protocols found.
2. Start `tak-server` with a test config (`config/test/CoreConfig.xml`) on isolated ports (defaults: 18087/18088/18089/26969 — not the production ports). Capture the server PID and stream stdout/stderr to `/tmp/tak-replay-<timestamp>.log`.
3. Connect a `taktool sub` subscriber to the server's stream port to capture all outputs.
4. Replay the pcap using `taktool replay <pcap>`:
   - Strips link-layer headers, extracts the TAK payload (TCP reassembly via `pcap-parser`).
   - Sends to the server at the original wire timing (or `--max-rate` if supplied).
5. After the pcap is exhausted, allow 2s for fan-out to complete, then stop the subscriber.
6. If `--verify-against` was supplied, diff the captured outputs against the expected JSON. Report mismatches.
7. Stop the server (`kill <pid>`).
8. Print a summary: packets replayed, events captured, mismatches (if verifying), server log highlights (warnings/errors).

Pre-flight:
- `tak-server` and `taktool` binaries must be built (`cargo build --release -p tak-server -p taktool`). If not, tell the user to build first.
- The pcap must contain TAK traffic (TCP on standard ports or UDP multicast on 239.2.3.1:6969). If neither, warn.
- This is local replay; no production server is touched.

Useful for:
- Regression testing after codec changes — does an exercise pcap still produce the same fan-out?
- Reproducing a customer issue locally.
- Bench fodder: a long pcap is more representative than synthetic load.
