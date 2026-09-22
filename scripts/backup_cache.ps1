# Back up cache.db (prices, SEC facts, insider trades, ...) with rotation.
# It's your only local copy of months of fetched data - losing it means
# re-downloading everything, which is slow and rate-limited.
#
#   .\scripts\backup_cache.ps1                  # backup now, keep last 14
#   .\scripts\backup_cache.ps1 -KeepDays 30      # keep last 30 days instead
param(
    [string]$CacheFile = "cache.db",
    [string]$BackupDir = "backups",
    [int]$KeepDays = 14
)

$ErrorActionPreference = "Stop"
Set-Location "$PSScriptRoot\.."

if (-not (Test-Path $CacheFile)) {
    Write-Host "No $CacheFile found - nothing to back up."
    exit 0
}

New-Item -ItemType Directory -Force $BackupDir | Out-Null
$Stamp = Get-Date -Format "yyyyMMdd_HHmmss"
$Dest = Join-Path $BackupDir "cache_$Stamp.db"

# SQLite may have a WAL file with unflushed writes; a plain file copy can miss
# them. Use SQLite's own backup command (via the app's cargo-installed sqlite3
# if present) when available, else fall back to a checkpoint-then-copy.
$sqlite3 = Get-Command sqlite3.exe -ErrorAction SilentlyContinue
if ($sqlite3) {
    & $sqlite3.Source $CacheFile ".backup '$Dest'"
} else {
    Write-Host "sqlite3.exe not found - copying the file directly (WAL checkpoint not forced)."
    Copy-Item $CacheFile $Dest
    foreach ($ext in "-wal", "-shm") {
        if (Test-Path "$CacheFile$ext") { Copy-Item "$CacheFile$ext" "$Dest$ext" }
    }
}

$SizeMB = [math]::Round((Get-Item $Dest).Length / 1MB, 1)
Write-Host "Backed up $CacheFile -> $Dest ($SizeMB MB)"

# Rotation: delete backups older than KeepDays.
$Cutoff = (Get-Date).AddDays(-$KeepDays)
Get-ChildItem $BackupDir -Filter "cache_*.db*" | Where-Object { $_.LastWriteTime -lt $Cutoff } | ForEach-Object {
    Write-Host "Removing old backup: $($_.Name)"
    Remove-Item $_.FullName -Force
}
