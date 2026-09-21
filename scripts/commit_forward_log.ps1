# Commit today's forward-test entry so git provides an external timestamp.
# Run after the morning job, BEFORE the market opens. Only touches forward_log/.
$ErrorActionPreference = "Stop"
Set-Location "$PSScriptRoot\.."
$Dir = if ($env:FORWARD_LOG_DIR) { $env:FORWARD_LOG_DIR } else { "forward_log" }

& .\target\release\quant-edge.exe --forward-verify   # refuse to commit a broken chain
if ($LASTEXITCODE -ne 0) { throw "forward log failed verification" }

git add $Dir
git diff --cached --quiet -- $Dir
if ($LASTEXITCODE -eq 0) {
    Write-Host "forward log: nothing new to commit"
} else {
    git commit -m "forward log $((Get-Date).ToUniversalTime().ToString('yyyy-MM-dd'))" -- $Dir
}
