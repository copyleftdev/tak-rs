---
description: Run the full invariant gauntlet — clippy, deny, machete, the dhat alloc test on tak-bus, and loom on dispatch. Report pass/fail per invariant from docs/invariants.md.
allowed-tools: Read, Write, Bash, Glob
---

Run every invariant check we have and produce a single pass/fail report.

The gauntlet (run in this order, fail-fast OFF — capture all results):

1. **Discipline (D1, D5):** `cargo clippy --workspace --all-targets --all-features -- -D warnings`. Captures unwrap/expect/panic/println bans.
2. **Discipline (D3, D4, D5):** `cargo deny check`. Captures banned crates (chrono, openssl, log, etc.).
3. **API (A2):** `cargo machete`. Captures unused deps that bloat the API surface.
4. **Hot path (H1):** `cargo test -p tak-bus --test no_alloc -- --ignored`. Asserts steady-state dispatch performs zero heap allocs (uses dhat).
5. **Concurrency (N1):** `RUSTFLAGS="--cfg loom" cargo test -p tak-bus --test loom_dispatch --release`. Model-checks bus dispatch under all schedules.
6. **Correctness (C1):** `cargo test -p tak-cot --test roundtrip`. Proptest for lossless XML round-trip.
7. **Correctness (C2):** `cargo test -p tak-proto --test snapshots`. Insta snapshots for proto wire stability.
8. **Correctness (C3):** `cargo test -p tak-auth --test bitvector`. Group bitvector AND vs reference impl.
9. **Concurrency (N3):** `grep -rn "tokio::spawn" crates/ --include='*.rs'` (must return zero matches in lib code; only `tak_server::tasks::spawn` allowed).
10. **Hot path (H2):** `grep -nE '(to_string|to_owned|String::from|to_vec)' crates/tak-cot/src/ | grep -v 'tests/'` (sanity check on the codec; flag any matches for human review).

Output format (one line per invariant):

```
## Invariant gauntlet — <git sha>

[PASS] D1 — clippy unwrap/expect/panic bans
[PASS] D3-D5 — cargo deny (chrono/openssl/log/lazy_static)
[FAIL] H1 — dhat alloc test: 3 allocations detected in dispatch loop
       crates/tak-bus/tests/no_alloc.rs:47 — see test output
[PASS] N1 — loom dispatch (12,840 schedules explored)
...

## Summary
- 9 PASS, 1 FAIL
- Blocking failures: H1
```

Pre-flight: if the workspace doesn't exist (no `Cargo.toml` at root), the gauntlet can't run — tell the user to scaffold first.

Skip silently any check whose target test/bench file doesn't exist yet (early in the project lifecycle, several won't exist). Mark these as `[SKIP]` not `[FAIL]`.
