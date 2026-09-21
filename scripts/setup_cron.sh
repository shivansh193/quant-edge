#!/usr/bin/env bash
# Quant Edge — install cron jobs (Linux / macOS)
# Adds morning (9:05 AM) and evening (4:35 PM ET) cron entries.
# Run once: bash scripts/setup_cron.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DAILY="$SCRIPT_DIR/daily.sh"

if [ ! -f "$DAILY" ]; then
    echo "ERROR: daily.sh not found at $DAILY" >&2
    exit 1
fi

chmod +x "$DAILY"

# Detect cron timezone — warn if not US Eastern
TZ_HINT=""
if command -v timedatectl &>/dev/null; then
    CURR_TZ="$(timedatectl show -p Timezone --value 2>/dev/null || true)"
    [ "$CURR_TZ" != "America/New_York" ] && \
        TZ_HINT="  # NOTE: system TZ=$CURR_TZ — adjust times for your timezone"
fi

# Build cron lines
MORNING_JOB="5  9  * * 1-5  $DAILY morning >> $SCRIPT_DIR/../logs/cron.log 2>&1$TZ_HINT"
EVENING_JOB="35 16 * * 1-5  $DAILY evening >> $SCRIPT_DIR/../logs/cron.log 2>&1$TZ_HINT"

# Remove any existing quant-edge entries and re-add
CURRENT="$(crontab -l 2>/dev/null | grep -v 'daily.sh' || true)"
(
    echo "$CURRENT"
    echo "$MORNING_JOB"
    echo "$EVENING_JOB"
) | crontab -

echo "Cron jobs installed:"
echo "  Morning : $MORNING_JOB"
echo "  Evening : $EVENING_JOB"
echo ""
echo "Verify with: crontab -l"
