#!/bin/sh
# Deploys origin/main when it moves. Run every minute by plainscreener-update.timer.
set -eu
cd /opt/plainscreener
git fetch -q origin main
if [ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] && [ "${1:-}" != force ]; then
  exit 0
fi
git reset -q --hard origin/main
# A failed build leaves the running container alone.
docker compose -f deploy/compose.yml build app
docker compose -f deploy/compose.yml up -d
echo "deployed $(git rev-parse --short HEAD)"
