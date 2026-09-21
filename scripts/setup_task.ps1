# Quant Edge — install Windows Task Scheduler jobs
# Creates two tasks: morning scan (9:05 AM) and evening update (4:35 PM), weekdays only.
# Run once as Administrator: .\scripts\setup_task.ps1

param(
    [string]$TaskPrefix = "QuantEdge",
    [string]$MorningTime = "09:05",
    [string]$EveningTime = "16:35"
)

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$DailyScript = Join-Path $ScriptDir "daily.ps1"

if (-not (Test-Path $DailyScript)) {
    Write-Error "daily.ps1 not found at $DailyScript"
    exit 1
}

$PwshExe = (Get-Command powershell.exe).Source

function Register-DailyTask {
    param([string]$Name, [string]$Time, [string]$Mode)

    $Action  = New-ScheduledTaskAction `
        -Execute $PwshExe `
        -Argument "-NonInteractive -WindowStyle Hidden -File `"$DailyScript`" -Mode $Mode"

    # Weekdays only (Monday–Friday)
    $Trigger = New-ScheduledTaskTrigger -Weekly `
        -DaysOfWeek Monday, Tuesday, Wednesday, Thursday, Friday `
        -At $Time

    $Settings = New-ScheduledTaskSettingsSet `
        -ExecutionTimeLimit (New-TimeSpan -Minutes 10) `
        -StartWhenAvailable `
        -RunOnlyIfNetworkAvailable

    if (Get-ScheduledTask -TaskName $Name -ErrorAction SilentlyContinue) {
        Unregister-ScheduledTask -TaskName $Name -Confirm:$false
        Write-Host "Removed existing task: $Name"
    }

    Register-ScheduledTask `
        -TaskName $Name `
        -Action   $Action `
        -Trigger  $Trigger `
        -Settings $Settings `
        -RunLevel Highest `
        -Force | Out-Null

    Write-Host "Registered task '$Name' at $Time (weekdays)"
}

Register-DailyTask "$TaskPrefix-Morning" $MorningTime "morning"
Register-DailyTask "$TaskPrefix-Evening" $EveningTime "evening"

Write-Host ""
Write-Host "Tasks installed. Verify with: Get-ScheduledTask -TaskName 'QuantEdge*'"
