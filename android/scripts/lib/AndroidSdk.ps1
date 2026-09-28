# Shared helpers of the device-test scripts (dot-sourced; Windows PowerShell 5.1).
# See docs/android.md 23.2.

Set-StrictMode -Version Latest

# The android/ folder (this file is android/scripts/lib/AndroidSdk.ps1).
function Get-AasAndroidDir {
    return (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
}

# The repository root (the parent of android/).
function Get-AasRepoRoot {
    return (Resolve-Path (Join-Path (Get-AasAndroidDir) '..')).Path
}

# The Android SDK: ANDROID_HOME, ANDROID_SDK_ROOT, then sdk.dir of android/local.properties
# (the same order as android/settings.gradle.kts).
function Get-AndroidSdkDir {
    foreach ($candidate in @($env:ANDROID_HOME, $env:ANDROID_SDK_ROOT)) {
        if ($candidate -and (Test-Path -LiteralPath $candidate -PathType Container)) {
            return $candidate
        }
    }
    $props = Join-Path (Get-AasAndroidDir) 'local.properties'
    if (Test-Path -LiteralPath $props -PathType Leaf) {
        foreach ($line in Get-Content -LiteralPath $props -Encoding UTF8) {
            if ($line -match '^\s*sdk\.dir\s*=\s*(.+)$') {
                # java.util.Properties escaping: "C\:\\Users\\me" is C:\Users\me.
                $dir = [regex]::Replace($Matches[1].Trim(), '\\(.)', '$1')
                if (Test-Path -LiteralPath $dir -PathType Container) {
                    return $dir
                }
            }
        }
    }
    throw 'Android SDK not found: set ANDROID_HOME or sdk.dir in android/local.properties (docs/android.md 9.1).'
}

function Get-SdkTool {
    param([Parameter(Mandatory)] [string]$RelativePath)
    $path = Join-Path (Get-AndroidSdkDir) $RelativePath
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "$path is missing (install it with sdkmanager, docs/android.md 23.2)."
    }
    return $path
}

function Get-AdbPath { return Get-SdkTool 'platform-tools\adb.exe' }

function Get-EmulatorPath { return Get-SdkTool 'emulator\emulator.exe' }

# Runs adb against one device only (never "the" device: other emulators may be attached).
# Returns stdout as a string; throws with stderr when adb fails.
function Invoke-Adb {
    param(
        [Parameter(Mandatory)] [string]$Serial,
        [Parameter(Mandatory, ValueFromRemainingArguments)] [string[]]$Arguments
    )
    return Invoke-AdbCommand -Arguments (@('-s', $Serial) + $Arguments)
}

# Runs adb with [Arguments] as they are (for commands about adb itself, such as `devices`).
# Through the process API, not `& adb`: Windows PowerShell 5.1 turns adb's stderr notes ("daemon
# not running; starting now") into terminating errors under $ErrorActionPreference = 'Stop'.
function Invoke-AdbCommand {
    param([Parameter(Mandatory)] [string[]]$Arguments)
    $adb = Get-AdbPath
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $adb
    $psi.Arguments = ($Arguments | ForEach-Object { ConvertTo-ProcessArgument $_ }) -join ' '
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    $process = [System.Diagnostics.Process]::Start($psi)
    $stderrTask = $process.StandardError.ReadToEndAsync()
    $stdout = $process.StandardOutput.ReadToEnd()
    $process.WaitForExit()
    $stderr = $stderrTask.Result
    if ($process.ExitCode -ne 0) {
        throw "adb $($Arguments -join ' ') failed ($($process.ExitCode)): $stderr $stdout"
    }
    return $stdout
}

# Quotes one argument for a Windows command line (CommandLineToArgvW rules).
function ConvertTo-ProcessArgument {
    param([Parameter(Mandatory)] [AllowEmptyString()] [string]$Value)
    if ($Value -ne '' -and $Value -notmatch '[\s"]') {
        return $Value
    }
    $escaped = [regex]::Replace($Value, '(\\*)"', '$1$1\"')
    $escaped = [regex]::Replace($escaped, '(\\+)$', '$1$1')
    return '"' + $escaped + '"'
}

# The device's state as adb reports it ("device", "offline", ...), or $null when adb does not
# list it.
function Get-DeviceState {
    param([Parameter(Mandatory)] [string]$Serial)
    $lines = (Invoke-AdbCommand -Arguments @('devices')) -split "`r?`n"
    foreach ($line in $lines) {
        $parts = $line -split '\s+'
        if ($parts.Count -ge 2 -and $parts[0] -eq $Serial) {
            return $parts[1]
        }
    }
    return $null
}
