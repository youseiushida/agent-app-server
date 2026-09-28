<#
.SYNOPSIS
Starts the headless emulator the device tests run on (docs/android.md 23.2).

.DESCRIPTION
Creates the AVD on first use, starts it on an explicit console port (so it never collides with
other emulators on the PC: it is always addressed as emulator-<Port>), waits until Android has
booted, and prepares it for UI tests (screen on and unlocked, animations off).

The system image must be installed first (sdkmanager treats ';' in arguments specially, so use a
package file):
    emulator
    system-images;android-36;google_apis;x86_64
    > sdkmanager --package_file=<that file>

Hardware acceleration must be usable (`emulator -accel-check`: WHPX on Windows). This script
does not change Windows features; it stops with the emulator's explanation instead.

Stop the emulator with stop-emulator.ps1 (or `adb -s emulator-<Port> emu kill`).
#>
[CmdletBinding()]
param(
    [string]$Avd = 'aas-e2e-api36',
    [string]$SystemImage = 'system-images;android-36;google_apis;x86_64',
    [string]$DeviceProfile = 'medium_phone',
    [int]$Port = 5580,
    [int]$MemoryMb = 3072,
    [int]$Cores = 4,
    # Rendering: 'host' uses the PC's GPU (fast; also works headless), 'swiftshader_indirect'
    # renders on the CPU (slow: System UI may stop responding while it boots).
    [ValidateSet('host', 'auto', 'swiftshader_indirect')]
    [string]$Gpu = 'host',
    [int]$BootTimeoutSeconds = 600
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lib\AndroidSdk.ps1')

$serial = "emulator-$Port"
$sdk = Get-AndroidSdkDir
$emulator = Get-EmulatorPath

if ((Get-DeviceState $serial) -eq 'device') {
    Write-Host "$serial is already running."
    return
}

# 1. Acceleration (WHPX). Without it the emulator is unusably slow for UI tests.
$accel = & $emulator -accel-check 2>&1 | Out-String
if ($LASTEXITCODE -ne 0) {
    throw "Hardware acceleration is not usable (emulator -accel-check exited $LASTEXITCODE):`n$accel`nEnable the Windows Hypervisor Platform yourself; this script does not change Windows features."
}
Write-Host ($accel.Trim())

# 2. The system image.
$imageDir = Join-Path $sdk ($SystemImage -replace ';', '\')
if (-not (Test-Path -LiteralPath (Join-Path $imageDir 'system.img'))) {
    throw "The system image $SystemImage is not installed ($imageDir). Install it with sdkmanager --package_file=<file listing it>."
}

# 3. The AVD (created once; other AVDs on the PC are left alone).
$avds = & $emulator -list-avds 2>$null
if ($avds -notcontains $Avd) {
    $avdmanager = Get-SdkTool 'cmdline-tools\latest\bin\avdmanager.bat'
    Write-Host "Creating the AVD $Avd ($SystemImage, $DeviceProfile)..."
    # "Do you wish to create a custom hardware profile?" -> no.
    'no' | & $avdmanager create avd --name $Avd --package "`"$SystemImage`"" --device $DeviceProfile
    if ($LASTEXITCODE -ne 0) {
        throw "avdmanager could not create $Avd (exit $LASTEXITCODE)."
    }
}

# 4. Start it headless on the fixed port. Output goes to a log next to the AVD's temp files.
$logDir = Join-Path ([System.IO.Path]::GetTempPath()) 'aas-emulator'
New-Item -ItemType Directory -Force -Path $logDir | Out-Null
$stdoutLog = Join-Path $logDir "$serial.log"
$stderrLog = Join-Path $logDir "$serial.err.log"
$arguments = @(
    '-avd', $Avd,
    '-port', $Port,
    '-no-window', '-no-audio', '-no-boot-anim',
    # Cold boot and no snapshot: every run starts from the same state.
    '-no-snapshot',
    '-memory', $MemoryMb,
    '-cores', $Cores,
    '-gpu', $Gpu
)
Write-Host "Starting $serial ($Avd, $MemoryMb MB); log: $stdoutLog"
Start-Process -FilePath $emulator -ArgumentList $arguments -WindowStyle Hidden `
    -RedirectStandardOutput $stdoutLog -RedirectStandardError $stderrLog | Out-Null

# 5. Wait for Android to finish booting.
$deadline = (Get-Date).AddSeconds($BootTimeoutSeconds)
$booted = $false
while ((Get-Date) -lt $deadline) {
    if ((Get-DeviceState $serial) -eq 'device') {
        $value = ''
        try {
            $value = (Invoke-Adb $serial shell getprop sys.boot_completed).Trim()
        } catch {
            # adbd restarts during boot; try again.
            $value = ''
        }
        if ($value -eq '1') {
            $booted = $true
            break
        }
    }
    Start-Sleep -Seconds 2
}
if (-not $booted) {
    throw "$serial did not boot within $BootTimeoutSeconds s (see $stdoutLog and $stderrLog)."
}

# 6. Ready for UI tests: screen on and unlocked, no animations.
Invoke-Adb $serial shell svc power stayon true | Out-Null
Invoke-Adb $serial shell input keyevent KEYCODE_WAKEUP | Out-Null
Invoke-Adb $serial shell wm dismiss-keyguard | Out-Null
foreach ($setting in 'window_animation_scale', 'transition_animation_scale', 'animator_duration_scale') {
    Invoke-Adb $serial shell settings put global $setting 0 | Out-Null
}
Write-Host "$serial is ready ($((Invoke-Adb $serial shell getprop ro.build.version.release).Trim()), API $((Invoke-Adb $serial shell getprop ro.build.version.sdk).Trim()))."
