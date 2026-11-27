#!/usr/bin/env bash
# Runs pasters. On grizzly this is what app@pasters.service starts, from the live release
# in /srv/apps/pasters; deployd builds each release (scripts/release-build.sh) before it
# gets here. Locally: build first, then ./start.sh.
cd "$(dirname "$(readlink -f "$0")")" || exit 1

if [ -f .env ]; then
    set -a
    # shellcheck disable=SC1091
    . ./.env
    set +a
fi

exec ./target/release/pasters
