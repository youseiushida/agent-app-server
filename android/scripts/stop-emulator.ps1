<#
.SYNOPSIS
Stops the emulator that start-emulator.ps1 started (docs/android.md 23.2).

.DESCRIPTION
Sends `emu kill` to emulator-<Port> only (other emulators and devices on the PC are left alone)
and waits until adb no longer lists it. The AVD and its data stay; the next start-emulator.ps1
cold-boots it again.
#>
[CmdletBinding()]
param(
    [int]$Port = 5580,
    [int]$StopTimeoutSeconds = 60
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lib\AndroidSdk.ps1')

$serial = "emulator-$Port"
if ($null -eq (Get-DeviceState $serial)) {
    Write-Host "$serial is not running."
    return
}
Invoke-Adb $serial emu kill | Out-Null
$deadline = (Get-Date).AddSeconds($StopTimeoutSeconds)
while ($null -ne (Get-DeviceState $serial)) {
    if ((Get-Date) -gt $deadline) {
        throw "$serial is still listed by adb $StopTimeoutSeconds s after 'emu kill'."
    }
    Start-Sleep -Seconds 1
}
Write-Host "$serial stopped."
