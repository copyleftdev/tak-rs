#!/usr/bin/env bash
# PostToolUse hook for Edit/Write.
#   *.toml under workspace      → run `cargo deny check` (BLOCKS on policy failure)
#   *.rs                        → rustfmt --check (advisory) + mark src-touched
#   anything in crates/ or src/ → mark src-touched for the Stop hook
#
# Fail-open everywhere except an actual deny-policy violation.

set -uo pipefail

input=$(cat)
file=$(printf '%s' "$input" | sed -nE 's/.*"file_path"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' | head -1)
[ -z "$file" ] && exit 0

repo_root="$(pwd)"
[ ! -f "$repo_root/CLAUDE.md" ] && exit 0   # not in tak-rs root, bail

# Mark src as touched for the Stop hook.
case "$file" in
  *crates/*|*/src/*)
    mkdir -p "$repo_root/.claude/state"
    touch "$repo_root/.claude/state/touched-src"
    ;;
esac

# Cargo.toml gate: enforce dep policy via cargo-deny. Blocking on failure.
case "$file" in
  *Cargo.toml)
    [ ! -f "$repo_root/Cargo.toml" ] && exit 0   # workspace not scaffolded yet
    if command -v cargo-deny >/dev/null 2>&1; then
      if out=$(cd "$repo_root" && cargo deny check 2>&1); then
        echo "[hook:post-edit] cargo-deny: ok"
      else
        echo "[hook:post-edit] cargo-deny FAILED — dep policy violated. Edit blocked."
        printf '%s\n' "$out" | tail -60
        exit 2   # exit code 2 → blocking error in Claude Code hook semantics
      fi
    else
      echo "[hook:post-edit] cargo-deny not installed; skipping policy check."
      echo "                  Install: cargo install --locked cargo-deny"
    fi
    ;;
esac

# Rust file: format check (advisory, never blocks).
case "$file" in
  *.rs)
    if command -v rustfmt >/dev/null 2>&1; then
      if ! rustfmt --check --edition 2021 "$file" >/dev/null 2>&1; then
        echo "[hook:post-edit] rustfmt: $file would change — run 'cargo fmt'"
      fi
    fi
    ;;
esac

exit 0
