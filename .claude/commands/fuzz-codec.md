---
description: Run a 5-minute cargo-fuzz session against the tak-cot decoders and report any crashes or new corpus entries.
argument-hint: [duration-seconds] [target-name]
allowed-tools: Read, Write, Bash, Glob
---

Run cargo-fuzz on the tak-cot codec to surface decoder crashes. Default duration is 300 seconds; override via the first argument. Default target is `decode_xml`; available targets: `decode_xml`, `decode_proto`, `framing_stream`, `framing_mesh`. Override via the second argument.

`$ARGUMENTS` may be empty (defaults), or `<seconds>`, or `<seconds> <target>`.

Process:

1. Confirm `cargo install cargo-fuzz` is available; if not, instruct the user to install it (`cargo install cargo-fuzz`).
2. Confirm the fuzz target exists at `crates/tak-cot/fuzz/fuzz_targets/<target>.rs`. If not, list available targets and stop.
3. Run: `cd crates/tak-cot && cargo +nightly fuzz run <target> -- -max_total_time=<seconds>`.
4. After the run:
   - Report any new crashes (files in `fuzz/artifacts/<target>/`).
   - Report new corpus entries (delta in `fuzz/corpus/<target>/` count).
   - For each crash, print the input bytes (hex) and the panic message from the fuzzer log.
5. **Do not auto-commit corpus or crash artifacts.** Tell the user; let them decide.

If a crash is found:
- Save a minimized reproducer: `cargo fuzz tmin <target> <crash-input>`.
- Suggest a regression test: paste the minimized input as a `#[test]` skeleton in `crates/tak-cot/tests/regression.rs`.

Defaults are conservative because cargo-fuzz can saturate a CPU. For deeper runs, suggest `/fuzz-codec 3600` (1 hour) or running overnight via `/loop`.
