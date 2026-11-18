#!/bin/sh
# Deploys origin/main when it differs from what's running. Run every minute by
# plainscreener-update.timer; a failed build is retried on the next run.
set -eu
cd /opt/plainscreener
git fetch -q origin main
target=$(git rev-parse origin/main)
if [ "$target" = "$(cat .deployed 2>/dev/null)" ] && [ "${1:-}" != force ]; then
  exit 0
fi
git reset -q --hard "$target"
# A failed build leaves the running container alone.
docker compose -f deploy/compose.yml build app
docker compose -f deploy/compose.yml up -d
echo "$target" > .deployed
echo "deployed $(git rev-parse --short HEAD)"
