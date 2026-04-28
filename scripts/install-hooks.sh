#!/usr/bin/env bash
# scripts/install-hooks.sh — wire .githooks/ into git.
#
# Run once after cloning. Idempotent.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

git config core.hooksPath .githooks
chmod +x .githooks/* 2>/dev/null || true

cat <<'EOF'
Git hooks installed.
  pre-commit  — fmt + clippy + deny + nextest (fast; runs on every commit)
  pre-push    — full gauntlet (slower; runs on every push)

Required tools (install once):

  cargo install --locked cargo-deny cargo-nextest cargo-machete

Optional but recommended:

  cargo install --locked cargo-fuzz cargo-llvm-cov

Slash commands available in Claude Code:
  /proto-sync       — refresh vendored .proto from upstream
  /bench-hot        — firehose criterion bench + delta vs baseline
  /fuzz-codec       — cargo-fuzz round on tak-cot
  /check-invariants — full gauntlet incl. loom + dhat
  /replay-pcap      — replay a real TAK pcap through tak-server
EOF
