# Daily US forward-test run.
#
# Schedule for ~18:00 India time on weekdays: that is 08:30 ET in summer (EDT) and
# 07:30 ET in winter (EST) - before the 09:30 ET open in both, so the signals use
# the previous close and the next-open entry convention holds.
#
#   .\scripts\daily_forward.ps1            # run + commit (no push)
#   .\scripts\daily_forward.ps1 -Push      # ...and push, so GitHub timestamps it
param([switch]$Push)

$ErrorActionPreference = "Stop"
Set-Location "$PSScriptRoot\.."

$Exe = if (Test-Path ".\target\release\quant-edge.exe") { ".\target\release\quant-edge.exe" } else { ".\target\debug\quant-edge.exe" }

& $Exe --morning --us
if ($LASTEXITCODE -ne 0) { throw "morning run failed" }

& .\scripts\commit_forward_log.ps1

if ($Push) { git push origin main }
