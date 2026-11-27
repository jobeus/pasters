#!/usr/bin/env bash
# Build the release binary into target/release/pasters. deployd runs this in each new
# release folder on grizzly; a failure here leaves the live release alone.
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")/.."

if [ -n "${DEPLOYD_CACHE:-}" ]; then
    # One cargo build cache for all releases, so a deploy only compiles what changed.
    CARGO_TARGET_DIR="$DEPLOYD_CACHE/target" cargo build --release --locked
    mkdir -p target/release
    cp "$DEPLOYD_CACHE/target/release/pasters" target/release/pasters
else
    cargo build --release --locked
fi
test -x target/release/pasters
