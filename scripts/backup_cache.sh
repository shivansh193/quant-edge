#!/usr/bin/env bash
# Back up cache.db with rotation. See backup_cache.ps1 for the Windows version.
set -euo pipefail
cd "$(dirname "$0")/.."

CACHE_FILE="${1:-cache.db}"
BACKUP_DIR="${BACKUP_DIR:-backups}"
KEEP_DAYS="${KEEP_DAYS:-14}"

if [ ! -f "$CACHE_FILE" ]; then
  echo "No $CACHE_FILE found - nothing to back up."
  exit 0
fi

mkdir -p "$BACKUP_DIR"
STAMP=$(date +%Y%m%d_%H%M%S)
DEST="$BACKUP_DIR/cache_$STAMP.db"

if command -v sqlite3 >/dev/null 2>&1; then
  sqlite3 "$CACHE_FILE" ".backup '$DEST'"
else
  echo "sqlite3 not found - copying the file directly (WAL checkpoint not forced)."
  cp "$CACHE_FILE" "$DEST"
  [ -f "$CACHE_FILE-wal" ] && cp "$CACHE_FILE-wal" "$DEST-wal"
  [ -f "$CACHE_FILE-shm" ] && cp "$CACHE_FILE-shm" "$DEST-shm"
fi

echo "Backed up $CACHE_FILE -> $DEST ($(du -h "$DEST" | cut -f1))"

find "$BACKUP_DIR" -name 'cache_*.db*' -mtime "+$KEEP_DAYS" -print -delete
