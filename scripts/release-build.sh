#!/usr/bin/env bash
# Build the release binary. Run by start.sh before every launch and by the deploy webhook
# before it restarts onto a new commit. A no-op when nothing changed.
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")/.."

cargo build --release --locked
test -x target/release/pasters
