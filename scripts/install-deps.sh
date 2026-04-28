#!/usr/bin/env bash
# scripts/install-deps.sh — install the cargo plugins the local hooks
# need. Safe to re-run: checks each tool first and skips if present.
#
# Issue: https://github.com/copyleftdev/tak-rs/issues/4
#
# Usage:
#   scripts/install-deps.sh           # interactive — prompts before each install
#   scripts/install-deps.sh --yes     # non-interactive (CI / first-time setup)
#   scripts/install-deps.sh --check   # only report status; never install
set -euo pipefail

ASSUME_YES=0
CHECK_ONLY=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        -y|--yes)   ASSUME_YES=1; shift ;;
        --check)    CHECK_ONLY=1; shift ;;
        -h|--help)
            cat <<EOF
install-deps.sh — install (or report) cargo plugins required by the
pre-commit / pre-push hooks.

Required:
  cargo-deny     — license + advisory + ban policy enforcement
  cargo-nextest  — faster, prettier test runner
  cargo-machete  — unused-dependency detector

Optional (skipped unless invoked with the corresponding flag):
  cargo-fuzz, cargo-llvm-cov

Options:
  -y, --yes      install missing tools without prompting
  --check        only report status; do not install
  -h, --help     show this help
EOF
            exit 0
            ;;
        *)  echo "unknown arg: $1" >&2; exit 1 ;;
    esac
done

REQUIRED=(cargo-deny cargo-nextest cargo-machete)

# ----------------------------------------------------------------------
# Check rustc + cargo are present
# ----------------------------------------------------------------------
if ! command -v cargo > /dev/null; then
    cat >&2 <<'EOF'
[install-deps] cargo not found on PATH.
Install via: https://rustup.rs/   (or: `curl https://sh.rustup.rs -sSf | sh`)
EOF
    exit 1
fi

# ----------------------------------------------------------------------
# Per-tool helpers
# ----------------------------------------------------------------------
have() {
    # Some plugins are invoked as `cargo <name>` and live as
    # `cargo-<name>` on PATH. Check both.
    command -v "$1" > /dev/null 2>&1 || command -v "cargo-${1#cargo-}" > /dev/null 2>&1
}

prompt_install() {
    local tool="$1"
    if [[ "$ASSUME_YES" -eq 1 ]]; then
        return 0
    fi
    read -r -p "[install-deps] install $tool? [Y/n] " ans
    [[ -z "$ans" || "$ans" =~ ^[Yy] ]]
}

install_one() {
    local tool="$1"
    echo "[install-deps] cargo install --locked $tool"
    cargo install --locked "$tool"
}

# ----------------------------------------------------------------------
# Walk the required list
# ----------------------------------------------------------------------
missing=()
for tool in "${REQUIRED[@]}"; do
    if have "$tool"; then
        echo "[install-deps] ✓ $tool"
    else
        echo "[install-deps] ✗ $tool — missing"
        missing+=("$tool")
    fi
done

if [[ "${#missing[@]}" -eq 0 ]]; then
    echo "[install-deps] all required tools present"
    exit 0
fi

if [[ "$CHECK_ONLY" -eq 1 ]]; then
    echo "[install-deps] --check only — leaving ${missing[*]} unmodified"
    exit 1
fi

for tool in "${missing[@]}"; do
    if prompt_install "$tool"; then
        install_one "$tool"
    else
        echo "[install-deps] skipped $tool — pre-commit will fail without it" >&2
    fi
done

echo "[install-deps] done"
