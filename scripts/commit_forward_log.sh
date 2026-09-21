#!/usr/bin/env bash
# Commit today's forward-test entry so git provides an external timestamp.
# Run after the morning job, BEFORE the market opens. Only touches forward_log/.
set -euo pipefail
cd "$(dirname "$0")/.."
DIR="${FORWARD_LOG_DIR:-forward_log}"
./target/release/quant-edge --forward-verify        # refuse to commit a broken chain
git add "$DIR"
if git diff --cached --quiet -- "$DIR"; then
  echo "forward log: nothing new to commit"
else
  git commit -m "forward log $(date -u +%F)" -- "$DIR"
fi
