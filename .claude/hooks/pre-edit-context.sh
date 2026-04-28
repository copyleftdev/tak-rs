#!/usr/bin/env bash
# PreToolUse hook for Edit/Write — injects invariant reminders into context for
# sensitive files (tak-cot, tak-bus, any Cargo.toml). Advisory only; never blocks.
#
# Reads tool input as JSON on stdin; stdout is added to model context.

set -uo pipefail

input=$(cat)
file=$(printf '%s' "$input" | sed -nE 's/.*"file_path"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' | head -1)
[ -z "$file" ] && exit 0

case "$file" in
  *crates/tak-cot/*)
    cat <<'EOF'
[hook:pre-edit] Editing tak-cot — codec invariants apply (docs/invariants.md):
  H2  Decoders BORROW from input: fn decode<'a>(&self, &'a [u8]) -> Result<View<'a>>
      No String::from / to_owned / to_vec on hot path.
  H3  Fan-out is Bytes::clone, never Vec::clone.
  C1  XML round-trip MUST be lossless on xmlDetail (proptest enforces).
  D1  No unwrap / expect / panic / todo in lib code.
Consider invoking the cot-codec-reviewer agent before merging.
EOF
    ;;
  *crates/tak-bus/*)
    cat <<'EOF'
[hook:pre-edit] Editing tak-bus — hot-path invariants apply (docs/invariants.md):
  H1  Steady-state dispatch is alloc-free (dhat test enforces).
  H3  Fan-out is Bytes::clone, never Vec::clone.
  H4  GroupBitvector intersect is [u64;4] AND, not arbitrary bigint.
  H5  Per-subscription mpsc is bounded.
  N1  Bus dispatch must pass loom model checking.
  N2  No std::sync::Mutex on hot path; use parking_lot or dashmap.
  N3  No tokio::spawn directly; use tak_server::tasks::spawn.
Consider invoking the hot-path-perf agent before merging.
EOF
    ;;
  *crates/tak-net/src/tls*|*crates/tak-net/src/auth*)
    cat <<'EOF'
[hook:pre-edit] Editing TLS / auth path — security invariants apply:
  C5  No code path may pass an unverified peer cert to tak-auth.
  D4  rustls only — openssl-sys / native-tls banned.
Any unsafe block here REQUIRES the unsafe-auditor agent.
EOF
    ;;
  *Cargo.toml)
    cat <<'EOF'
[hook:pre-edit] Editing a Cargo.toml — banned crates per docs/invariants.md:
  D3  Time:    chrono, time           → use jiff
  D4  TLS:     openssl, openssl-sys, native-tls → use rustls
  D5  Logging: log, env_logger        → use tracing
  N3  XML:     xml-rs                 → use quick-xml
       Static: lazy_static            → use std::sync::OnceLock / LazyLock
       Async:  async-std, smol        → use tokio
`cargo deny check` will run automatically after this edit.
EOF
    ;;
  *deny.toml|*clippy.toml|*rust-toolchain.toml|*.cargo/config.toml)
    cat <<'EOF'
[hook:pre-edit] Editing a project gate config. These files codify the policies
in docs/invariants.md. Loosening a gate (allowing a banned crate, raising a
clippy threshold) requires a written rationale in the commit message.
EOF
    ;;
esac

exit 0
