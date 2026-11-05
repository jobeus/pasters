#!/usr/bin/env bash
# Runs pasters, relaunching it whenever it exits with code 42. The deploy webhook exits
# that way after it has `git pull`ed and built the new commit, so the build below is
# normally a no-op; it only does real work after a manual pull or on first start.
# Any other exit code ends the loop (systemd's Restart=always covers crashes).
cd "$(dirname "$0")" || exit 1

while true; do
    if ! cargo build --release --locked; then
        echo "start.sh: build failed, starting the previous binary" >&2
    fi
    if [ ! -x target/release/pasters ]; then
        echo "start.sh: no binary at target/release/pasters" >&2
        exit 1
    fi

    ./target/release/pasters
    code=$?
    echo "start.sh: pasters exited with code $code"

    if [ "$code" -ne 42 ]; then
        exit "$code"
    fi
    echo "start.sh: restarting onto the new build"
    sleep 1
done
