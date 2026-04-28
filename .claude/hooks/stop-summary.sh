#!/usr/bin/env bash
# Stop hook — runs at end of session/turn. If source was touched, remind to
# run the invariant gauntlet before committing. Advisory only; never blocks.

set -uo pipefail

repo_root="$(pwd)"
[ ! -f "$repo_root/CLAUDE.md" ] && exit 0
marker="$repo_root/.claude/state/touched-src"
[ ! -f "$marker" ] && exit 0

rm -f "$marker"

cat <<'EOF'
[hook:stop] Source was modified this session. Before committing, run:
  /check-invariants    — clippy + deny + machete + dhat alloc + loom + roundtrips
  /bench-hot           — verify firehose perf hasn't regressed (if hot path touched)
EOF

exit 0
