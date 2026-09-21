#!/usr/bin/env bash
# Quant Edge — daily automation (Linux / macOS)
# Runs morning scan or evening update depending on time (or explicit mode).
# Designed to be called by cron; see setup_cron.sh.

set -euo pipefail

HOST="${QUANT_EDGE_HOST:-http://localhost:8080}"
MODE="${1:-auto}"   # morning | evening | auto
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOG_DIR="$SCRIPT_DIR/../logs"
DATE="$(date +%Y%m%d)"
HOUR="$(date +%H)"

mkdir -p "$LOG_DIR"

invoke_api() {
    local endpoint="$1"
    local label="$2"
    local url="$HOST/api/$endpoint"
    echo "[$label] Calling $url"
    if curl -sf --max-time 120 "$url" -o "$LOG_DIR/${label}_${DATE}.json"; then
        echo "[$label] Saved to logs/${label}_${DATE}.json"
    else
        echo "[$label] WARNING: request failed (exit $?)" >&2
    fi
}

invoke_paper_update() {
    local url="$HOST/api/paper/update"
    echo "[paper-update] Calling $url"
    curl -sf --max-time 60 -X POST "$url" > /dev/null && echo "[paper-update] Done" \
        || echo "[paper-update] WARNING: request failed" >&2
}

case "$MODE" in
    morning)
        invoke_api "morning" "morning"
        invoke_paper_update
        ;;
    evening)
        invoke_api "evening" "evening"
        invoke_paper_update
        ;;
    auto)
        if [ "$HOUR" -ge 9 ] && [ "$HOUR" -lt 12 ]; then
            invoke_api "morning" "morning"
            invoke_paper_update
        elif [ "$HOUR" -ge 16 ]; then
            invoke_api "evening" "evening"
            invoke_paper_update
        else
            echo "Nothing to do at $(date +%H:%M)"
        fi
        ;;
    *)
        echo "Usage: $0 [morning|evening|auto]" >&2
        exit 1
        ;;
esac
