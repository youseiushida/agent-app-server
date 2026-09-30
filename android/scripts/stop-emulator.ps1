<#
.SYNOPSIS
Stops the emulator that start-emulator.ps1 started (docs/android.md 23.2).

.DESCRIPTION
Sends `emu kill` to emulator-<Port> only (other emulators and devices on the PC are left alone)
and waits until adb no longer lists it. When the emulator does not stop within
-StopTimeoutSeconds (its console can stop answering, e.g. after the emulated system crashed),
stops the processes of that emulator only: the emulator and QEMU processes whose command line
names `-port <Port>`, as start-emulator.ps1 starts them. Nothing is stopped by image name. The
AVD and its data stay; the next start-emulator.ps1 cold-boots it again.
#>
[CmdletBinding()]
param(
    [int]$Port = 5580,
    [int]$StopTimeoutSeconds = 60
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lib\AndroidSdk.ps1')

function Wait-Gone([string]$Serial, [int]$TimeoutSeconds) {
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ($null -ne (Get-DeviceState $Serial)) {
        if ((Get-Date) -gt $deadline) { return $false }
        Start-Sleep -Seconds 1
    }
    return $true
}

$serial = "emulator-$Port"
if ($null -eq (Get-DeviceState $serial)) {
    Write-Host "$serial is not running."
    return
}
Invoke-Adb $serial emu kill | Out-Null
if (Wait-Gone $serial $StopTimeoutSeconds) {
    Write-Host "$serial stopped."
    return
}

# `emu kill` was not obeyed: stop this emulator's own processes (QEMU first; the emulator
# launcher follows it). They are the ones started with this port.
$pattern = "* -port $Port *"
$own = @(Get-CimInstance Win32_Process | Where-Object {
        $_.CommandLine -and $_.CommandLine -like $pattern -and ($_.Name -like 'qemu-system*' -or $_.Name -eq 'emulator.exe')
    })
if ($own.Count -eq 0) {
    throw "$serial is still listed by adb $StopTimeoutSeconds s after 'emu kill', and no emulator process with -port $Port runs."
}
foreach ($process in ($own | Sort-Object { if ($_.Name -like 'qemu-system*') { 0 } else { 1 } })) {
    Write-Host "$serial did not obey 'emu kill': stopping $($process.Name) ($($process.ProcessId))."
    try {
        Stop-Process -Id $process.ProcessId -Force -Confirm:$false -ErrorAction Stop
    } catch [Microsoft.PowerShell.Commands.ProcessCommandException] {
        # It ended by itself meanwhile (the launcher exits with QEMU).
    }
}
if (-not (Wait-Gone $serial $StopTimeoutSeconds)) {
    throw "$serial is still listed by adb after its processes were stopped."
}
Write-Host "$serial stopped (its processes were stopped)."
