# Daily US forward-test run.
#
# Schedule for ~18:00 India time on weekdays: that is 08:30 ET in summer (EDT) and
# 07:30 ET in winter (EST) - before the 09:30 ET open in both, so the signals use
# the previous close and the next-open entry convention holds.
#
# This is a SECOND runner alongside .github/workflows/forward-log.yml (which
# runs on GitHub's servers regardless of whether this laptop is on). Both are
# safe to keep running: this script pulls first, so if Actions already
# recorded today's entry, quant-edge's own append-only check (record() will
# not overwrite or back-date) makes the local run a clean no-op rather than a
# conflict or a duplicate entry.
#
#   .\scripts\daily_forward.ps1            # run + commit (no push)
#   .\scripts\daily_forward.ps1 -Push      # ...and push, so GitHub timestamps it
param([switch]$Push)

$ErrorActionPreference = "Stop"
Set-Location "$PSScriptRoot\.."

# $ErrorActionPreference only catches PowerShell cmdlet errors, not a native
# exe's exit code - git/cargo/quant-edge failures need an explicit check or
# the script silently carries on with stale code/data.
git pull --ff-only origin main
if ($LASTEXITCODE -ne 0) { throw "git pull failed" }

$Today = Get-Date -Format "yyyy-MM-dd"
$Dir = if ($env:FORWARD_LOG_DIR) { $env:FORWARD_LOG_DIR } else { "forward_log" }
if (Test-Path "$Dir\$Today.json") {
    Write-Host "Today's forward-log entry already exists (recorded by another runner) - nothing to do."
    exit 0
}

# Always rebuild against whatever `git pull` just brought in. A binary from
# an old commit doesn't just risk being "behind" - it can flat-out reject
# CLI flags the current script passes (found 2026-09-26: a release build
# from May silently failed every local run with "unexpected argument '--us'"
# while GitHub Actions, which builds fresh every time, covered for it).
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "release build failed" }
$Exe = ".\target\release\quant-edge.exe"

& $Exe --morning --us
if ($LASTEXITCODE -ne 0) { throw "morning run failed" }

& .\scripts\commit_forward_log.ps1

if ($Push) { git push origin main }
