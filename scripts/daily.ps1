# Quant Edge — daily automation (Windows PowerShell)
# Runs the morning scan at 9:00 AM and evening update at 4:30 PM.
# Designed to be invoked by Task Scheduler; see setup_task.ps1.

param(
    [string]$Host = "http://localhost:8080",
    [string]$Mode = "auto"   # "morning" | "evening" | "auto"
)

$LogDir = "$PSScriptRoot\..\logs"
if (-not (Test-Path $LogDir)) { New-Item -ItemType Directory -Force $LogDir | Out-Null }

$Now  = Get-Date
$Hour = $Now.Hour

function Invoke-Api {
    param([string]$Endpoint, [string]$Label)
    $Url = "$Host/api/$Endpoint"
    Write-Host "[$Label] Calling $Url"
    try {
        $Response = Invoke-WebRequest -Uri $Url -Method GET -TimeoutSec 120 -UseBasicParsing
        $OutFile  = "$LogDir\${Label}_$($Now.ToString('yyyyMMdd')).json"
        $Response.Content | Out-File -FilePath $OutFile -Encoding utf8
        Write-Host "[$Label] Saved to $OutFile"
    } catch {
        Write-Warning "[$Label] Failed: $_"
    }
}

function Invoke-PaperUpdate {
    $Url = "$Host/api/paper/update"
    Write-Host "[paper-update] Calling $Url"
    try {
        Invoke-WebRequest -Uri $Url -Method POST -TimeoutSec 60 -UseBasicParsing | Out-Null
        Write-Host "[paper-update] Done"
    } catch {
        Write-Warning "[paper-update] Failed: $_"
    }
}

switch ($Mode) {
    "morning" {
        Invoke-Api "morning" "morning"
        Invoke-PaperUpdate
    }
    "evening" {
        Invoke-Api "evening" "evening"
        Invoke-PaperUpdate
    }
    "auto" {
        if ($Hour -ge 9 -and $Hour -lt 12) {
            Invoke-Api "morning" "morning"
            Invoke-PaperUpdate
        } elseif ($Hour -ge 16) {
            Invoke-Api "evening" "evening"
            Invoke-PaperUpdate
        } else {
            Write-Host "Nothing to do at $($Now.ToString('HH:mm'))"
        }
    }
}
