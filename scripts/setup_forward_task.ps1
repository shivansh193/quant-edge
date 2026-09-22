# Register the daily US forward-test job in Windows Task Scheduler.
# Run once, from the project root:   .\scripts\setup_forward_task.ps1
# Remove later with:                 Unregister-ScheduledTask QuantEdgeForward -Confirm:$false
param([string]$At = "18:00")   # local (IST) time; see daily_forward.ps1 for why

$Script = Join-Path $PSScriptRoot "daily_forward.ps1"
$Action = New-ScheduledTaskAction -Execute (Get-Command powershell.exe).Source `
    -Argument "-NonInteractive -WindowStyle Hidden -File `"$Script`" -Push"
$Trigger = New-ScheduledTaskTrigger -Weekly -DaysOfWeek Monday,Tuesday,Wednesday,Thursday,Friday -At $At
$Settings = New-ScheduledTaskSettingsSet -StartWhenAvailable -RunOnlyIfNetworkAvailable `
    -ExecutionTimeLimit (New-TimeSpan -Minutes 40)

Register-ScheduledTask -TaskName "QuantEdgeForward" -Action $Action -Trigger $Trigger -Settings $Settings -Force | Out-Null
Write-Host "Registered QuantEdgeForward: weekdays at $At local time."
