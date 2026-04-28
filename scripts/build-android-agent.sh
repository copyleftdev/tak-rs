#!/usr/bin/env bash
# Cross-compile `tak-agent` for Android.
#
# Usage:
#   ./scripts/build-android-agent.sh                # default: aarch64
#   TARGET=armv7-linux-androideabi ./scripts/build-android-agent.sh
#
# Requires: Android NDK + cargo-ndk (`cargo install cargo-ndk`).
# `cargo-ndk` is the path-of-least-resistance for NDK linker setup;
# the manual alternative is to set CARGO_TARGET_<TARGET>_LINKER and
# CC_<target> by hand against the NDK clang wrappers.
#
# Output: target/<TARGET>/release/tak-agent

set -euo pipefail

TARGET="${TARGET:-aarch64-linux-android}"
ANDROID_API="${ANDROID_API:-26}"   # Android 8.0; covers ATAK CIV minimum

if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo not on PATH" >&2
    exit 127
fi

# Make sure the rustup target is installed; cargo build will error
# anyway, but a clear up-front message saves a confusing log.
if ! rustup target list --installed | grep -qx "$TARGET"; then
    echo "==> rustup target add $TARGET"
    rustup target add "$TARGET"
fi

if command -v cargo-ndk >/dev/null 2>&1; then
    echo "==> cargo ndk -t $TARGET -p $ANDROID_API build --release -p tak-agent"
    cargo ndk -t "$TARGET" -p "$ANDROID_API" build --release -p tak-agent
else
    cat <<'EOF' >&2
cargo-ndk not on PATH.

Install with:
    cargo install cargo-ndk

You also need the Android NDK installed and ANDROID_NDK_HOME set
(or ~/Android/Sdk/ndk/<version> on a typical Studio install).

Falling back to a plain `cargo build` for the target — this only
works if you've already configured the NDK linker manually in
.cargo/config.toml.
EOF
    cargo build --release -p tak-agent --target "$TARGET"
fi

OUT="target/$TARGET/release/tak-agent"
if [[ ! -f "$OUT" ]]; then
    echo "build did not produce $OUT" >&2
    exit 1
fi

ls -la "$OUT"
file "$OUT" 2>/dev/null || true
echo
echo "Next: ./scripts/run-agent-scenario.sh $OUT all"
