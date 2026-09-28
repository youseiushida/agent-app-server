<#
.SYNOPSIS
Runs the instrumented test suite on a device or emulator against the real daemon
(docs/android.md 23.2).

.DESCRIPTION
1. Starts target\aas-test-bin\aas-test-server.exe (the real daemon with the fake harness behind a
   chaos proxy) on a fresh temporary state folder and reads its ready line.
2. `adb reverse`s the proxy's port, so the app on the device reaches the host's 127.0.0.1, and
   the port of a small control channel this script serves (below).
3. Runs `gradlew :e2e:connected<Build>AndroidTest` (the self-instrumenting device tests, which
   install the app of the same build type) for each requested build type (debug, and the
   R8-processed staging build), passing the server through instrumentation arguments:
     wsUrl, httpUrl, pairingCode   as printed by the server
     root                          the project root, base64url (UTF-8): the device shell that
                                   runs `am instrument` would eat the backslashes of a Windows path
     controlPort                   the control channel
     screenshots                   "true" when -ScreenshotDir is given
4. Always quits the server (kills it if it does not stop), removes the reverse rules it added and
   deletes the state folder (unless -KeepState).

The control channel (test-only): one request per TCP connection to 127.0.0.1:<controlPort> on
the device (reversed to this script). The client sends command lines (UTF-8) ending with an empty
line; the script answers with JSON lines and closes the connection. Commands:
  chaos pass|drop|blackhole|delay <ms>, restart, reset, pairing-code, native-session ...
      forwarded to the server's stdin; the answer is every line the server printed for it,
      ending with its {"event":"ok"|"error",...} line
  ready             the server's latest ready line
  screenshot <name> `adb exec-out screencap -p` into <ScreenshotDir>\<name>.png
                    ({"event":"ok","skipped":true} without -ScreenshotDir)
The server's own `quit` is refused: this script owns the server's lifetime.

Exit code: 0 when every run passed.

.EXAMPLE
.\start-emulator.ps1
.\run-device-tests.ps1 -BuildType both -Repeat 2 -ScreenshotDir C:\tmp\screens
.\stop-emulator.ps1
#>
[CmdletBinding()]
param(
    [string]$Serial = 'emulator-5580',
    [ValidateSet('debug', 'staging', 'both')]
    [string]$BuildType = 'both',
    [ValidateRange(1, 20)]
    [int]$Repeat = 1,
    # A test class or class#method (instrumentation argument "class"); all tests when empty.
    [string]$Tests = '',
    [string]$ScreenshotDir = '',
    [string]$ServerExe = '',
    [int]$GradleTimeoutMinutes = 90,
    # Upper bound for the server's ready line and for one forwarded command (a restart stops and
    # starts the daemon; the first start seeds native sessions).
    [int]$ServerTimeoutSeconds = 120,
    [string]$ResultsDir = '',
    [switch]$KeepState
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lib\AndroidSdk.ps1')

$androidDir = Get-AasAndroidDir
$repoRoot = Get-AasRepoRoot
if (-not $ServerExe) {
    $ServerExe = Join-Path $repoRoot 'target\aas-test-bin\aas-test-server.exe'
}
if (-not (Test-Path -LiteralPath $ServerExe -PathType Leaf)) {
    throw "$ServerExe is missing: build the test server snapshot first (README, docs/android.md 9.2)."
}
if (-not $ResultsDir) {
    $ResultsDir = Join-Path $androidDir 'e2e\build\device-test-results'
}
if ($ScreenshotDir) {
    New-Item -ItemType Directory -Force -Path $ScreenshotDir | Out-Null
    $ScreenshotDir = (Resolve-Path -LiteralPath $ScreenshotDir).Path
}
$state = Get-DeviceState $Serial
if ($state -ne 'device') {
    throw "$Serial is not ready (adb state: $state). Start it with start-emulator.ps1."
}
$adb = Get-AdbPath
$utf8 = New-Object System.Text.UTF8Encoding($false)

# --- The test server -------------------------------------------------------------------------

$stateDir = Join-Path ([System.IO.Path]::GetTempPath()) ("aas-device-tests-" + [Guid]::NewGuid().ToString('N').Substring(0, 12))
New-Item -ItemType Directory -Force -Path $stateDir | Out-Null

$server = $null
$pendingRead = $null
$latestReady = $null
$reversed = New-Object System.Collections.Generic.List[string]
$listener = $null

# Reads one line of the server's stdout within $TimeoutMs, or $null on timeout. A read that timed
# out stays pending and is picked up by the next call (StreamReader allows one read at a time).
function Read-ServerLine {
    param([int]$TimeoutMs)
    if ($null -eq $script:pendingRead) {
        $script:pendingRead = $script:server.StandardOutput.ReadLineAsync()
    }
    if (-not $script:pendingRead.Wait($TimeoutMs)) {
        return $null
    }
    $line = $script:pendingRead.Result
    $script:pendingRead = $null
    if ($null -eq $line) {
        throw "the test server closed its output (exit code: $(if ($script:server.HasExited) { $script:server.ExitCode } else { 'still running' }))"
    }
    return $line
}

# Sends one command to the server and returns every line it printed for it (the last one is its
# ok/error line).
function Invoke-ServerCommand {
    param([string]$Command)
    $script:server.StandardInput.Write($Command + "`n")
    $script:server.StandardInput.Flush()
    $lines = New-Object System.Collections.Generic.List[string]
    $deadline = (Get-Date).AddSeconds($ServerTimeoutSeconds)
    while ($true) {
        $left = [int][Math]::Max(0, ($deadline - (Get-Date)).TotalMilliseconds)
        if ($left -eq 0) {
            throw "the test server did not finish '$Command' within $ServerTimeoutSeconds s"
        }
        $line = Read-ServerLine -TimeoutMs $left
        if ($null -eq $line) { continue }
        $lines.Add($line)
        $event = $null
        try { $event = $line | ConvertFrom-Json } catch { $event = $null }
        if ($null -ne $event -and $event.event -eq 'ready') {
            $script:latestReady = $line
        }
        if ($null -ne $event -and ($event.event -eq 'ok' -or $event.event -eq 'error') -and $event.cmd -eq $Command) {
            return $lines
        }
    }
}

function Save-Screenshot {
    param([string]$Name)
    $target = Join-Path $ScreenshotDir "$Name.png"
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $adb
    $psi.Arguments = "-s $Serial exec-out screencap -p"
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    $process = [System.Diagnostics.Process]::Start($psi)
    $stderrTask = $process.StandardError.ReadToEndAsync()
    $file = [System.IO.File]::Create($target)
    try {
        $process.StandardOutput.BaseStream.CopyTo($file)
    } finally {
        $file.Dispose()
    }
    $process.WaitForExit()
    if ($process.ExitCode -ne 0) {
        throw "screencap failed ($($process.ExitCode)): $($stderrTask.Result)"
    }
    return $target
}

function ConvertTo-JsonLine {
    param([hashtable]$Value)
    return ($Value | ConvertTo-Json -Compress)
}

# Runs one control command and returns the JSON lines of its answer.
function Invoke-ControlCommand {
    param([string]$Command)
    $words = $Command -split '\s+'
    switch ($words[0]) {
        'ready' {
            return @($script:latestReady)
        }
        'screenshot' {
            if ($words.Count -ne 2 -or $words[1] -notmatch '^[A-Za-z0-9_.-]+$') {
                return @(ConvertTo-JsonLine @{ event = 'error'; cmd = $Command; message = 'usage: screenshot <name> ([A-Za-z0-9_.-])' })
            }
            if (-not $ScreenshotDir) {
                return @(ConvertTo-JsonLine @{ event = 'ok'; cmd = $Command; skipped = $true })
            }
            $path = Save-Screenshot -Name $words[1]
            Write-Host "  [control] screenshot $path"
            return @(ConvertTo-JsonLine @{ event = 'ok'; cmd = $Command; path = $path })
        }
        'quit' {
            return @(ConvertTo-JsonLine @{ event = 'error'; cmd = $Command; message = 'the script owns the server; quit is not allowed' })
        }
        default {
            Write-Host "  [control] $Command"
            return (Invoke-ServerCommand -Command $Command)
        }
    }
}

# Serves one control connection: command lines until an empty line, then the answers.
function Invoke-ControlConnection {
    param([System.Net.Sockets.TcpClient]$Client)
    try {
        $Client.ReceiveTimeout = 30000
        $stream = $Client.GetStream()
        $reader = New-Object System.IO.StreamReader($stream, $utf8)
        $writer = New-Object System.IO.StreamWriter($stream, $utf8)
        $writer.NewLine = "`n"
        $commands = New-Object System.Collections.Generic.List[string]
        while ($true) {
            $line = $reader.ReadLine()
            if ($null -eq $line -or $line.Trim() -eq '') { break }
            $commands.Add($line.Trim())
        }
        foreach ($command in $commands) {
            try {
                foreach ($answer in (Invoke-ControlCommand -Command $command)) {
                    $writer.WriteLine($answer)
                }
            } catch {
                $writer.WriteLine((ConvertTo-JsonLine @{ event = 'error'; cmd = $command; message = "$_" }))
            }
        }
        $writer.Flush()
    } catch {
        Write-Warning "control connection failed: $_"
    } finally {
        $Client.Close()
    }
}

function Add-Reverse {
    param([int]$Port)
    Invoke-Adb $Serial reverse "tcp:$Port" "tcp:$Port" | Out-Null
    $script:reversed.Add("tcp:$Port")
}

# Counts the results of one connected run (JUnit XML written by the Android Gradle plugin).
function Get-RunSummary {
    param([string]$Dir)
    $tests = 0; $failures = 0; $errors = 0; $skipped = 0
    foreach ($file in Get-ChildItem -LiteralPath $Dir -Recurse -Filter 'TEST-*.xml' -ErrorAction SilentlyContinue) {
        [xml]$xml = Get-Content -LiteralPath $file.FullName -Encoding UTF8
        $suites = @($xml.SelectNodes('//testsuite'))
        if ($suites.Count -eq 0) { $suites = @($xml.SelectNodes('/testsuites')) }
        foreach ($suite in $suites) {
            $tests += [int]$suite.tests
            $failures += [int]$suite.failures
            $errors += [int]$suite.errors
            $skipped += [int]$suite.skipped
        }
        # The first lines of each failure, so the console tells what broke.
        foreach ($case in @($xml.SelectNodes('//testcase[failure or error]'))) {
            $problem = @($case.SelectNodes('failure|error'))[0].InnerText.Trim() -split "`n" | Select-Object -First 3
            Write-Host "  FAILED $($case.classname).$($case.name): $($problem -join ' / ')"
        }
    }
    return "tests=$tests failures=$failures errors=$errors skipped=$skipped"
}

function Invoke-GradleRun {
    param([string]$Build, [hashtable]$Ready, [int]$ControlPort, [string]$LogFile)
    $variant = $Build.Substring(0, 1).ToUpperInvariant() + $Build.Substring(1)
    $rootB64 = [Convert]::ToBase64String($utf8.GetBytes($Ready.root)).TrimEnd('=').Replace('+', '-').Replace('/', '_')
    $runnerArgs = [ordered]@{
        wsUrl = $Ready.wsUrl
        httpUrl = $Ready.httpUrl
        pairingCode = $Ready.pairingCode
        root = $rootB64
        controlPort = $ControlPort
        screenshots = $(if ($ScreenshotDir) { 'true' } else { 'false' })
    }
    if ($Tests) { $runnerArgs['class'] = $Tests }
    $arguments = New-Object System.Collections.Generic.List[string]
    $arguments.Add('--max-workers=6')
    $arguments.Add(":e2e:connected${variant}AndroidTest")
    foreach ($key in $runnerArgs.Keys) {
        $arguments.Add("-Pandroid.testInstrumentationRunnerArguments.$key=$($runnerArgs[$key])")
    }
    $gradlew = Join-Path $androidDir 'gradlew.bat'
    $commandLine = (@($gradlew) + $arguments | ForEach-Object { ConvertTo-ProcessArgument $_ }) -join ' '
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    # Through cmd for the redirection: Gradle's output goes to the run's log, not this console.
    $psi.FileName = Join-Path $env:SystemRoot 'System32\cmd.exe'
    $psi.Arguments = '/d /s /c "' + $commandLine + ' > ' + (ConvertTo-ProcessArgument $LogFile) + ' 2>&1"'
    $psi.WorkingDirectory = $androidDir
    $psi.UseShellExecute = $false
    # Gradle's connected tasks run on every attached device unless ANDROID_SERIAL names one.
    $psi.EnvironmentVariables['ANDROID_SERIAL'] = $Serial
    Write-Host "gradlew $($arguments -join ' ')"
    Write-Host "  (Gradle's output: $LogFile)"
    $gradle = [System.Diagnostics.Process]::Start($psi)
    $deadline = (Get-Date).AddMinutes($GradleTimeoutMinutes)
    while (-not $gradle.HasExited) {
        if ((Get-Date) -gt $deadline) {
            & taskkill.exe /T /F /PID $gradle.Id | Out-Null
            throw "gradle did not finish within $GradleTimeoutMinutes minutes"
        }
        if ($script:listener.Pending()) {
            Invoke-ControlConnection -Client $script:listener.AcceptTcpClient()
        } else {
            Start-Sleep -Milliseconds 20
        }
    }
    $gradle.WaitForExit()
    return $gradle.ExitCode
}

$exitCode = 0
try {
    # Start the server.
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $ServerExe
    $psi.Arguments = '--state-dir ' + (ConvertTo-ProcessArgument $stateDir)
    $psi.UseShellExecute = $false
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.StandardOutputEncoding = $utf8
    # stderr (the server's warnings) stays on this console.
    # .NET writes the stdin encoding's preamble when the process starts: without this, the server's
    # first command would begin with a byte order mark (and be unknown to it).
    $consoleInputEncoding = [Console]::InputEncoding
    [Console]::InputEncoding = $utf8
    try {
        $server = [System.Diagnostics.Process]::Start($psi)
    } finally {
        [Console]::InputEncoding = $consoleInputEncoding
    }
    $readyLine = $null
    $deadline = (Get-Date).AddSeconds($ServerTimeoutSeconds)
    while ($null -eq $readyLine) {
        $left = [int][Math]::Max(0, ($deadline - (Get-Date)).TotalMilliseconds)
        if ($left -eq 0) { throw "the test server did not print its ready line within $ServerTimeoutSeconds s" }
        $line = Read-ServerLine -TimeoutMs $left
        if ($null -ne $line -and ($line | ConvertFrom-Json).event -eq 'ready') { $readyLine = $line }
    }
    $latestReady = $readyLine
    $readyObject = $readyLine | ConvertFrom-Json
    $ready = @{ wsUrl = $readyObject.wsUrl; httpUrl = $readyObject.httpUrl; pairingCode = $readyObject.pairingCode; root = $readyObject.root }
    Write-Host "test server ready: $($ready.wsUrl) (state $stateDir)"

    # The device reaches the host's loopback through adb reverse.
    $proxyPort = ([Uri]$ready.httpUrl).Port
    Add-Reverse -Port $proxyPort
    $listener = New-Object System.Net.Sockets.TcpListener([System.Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $controlPort = ([System.Net.IPEndPoint]$listener.LocalEndpoint).Port
    Add-Reverse -Port $controlPort
    Write-Host "reversed tcp:$proxyPort (server) and tcp:$controlPort (control) on $Serial"

    # UI tests want no animations and an awake, unlocked screen.
    foreach ($setting in 'window_animation_scale', 'transition_animation_scale', 'animator_duration_scale') {
        Invoke-Adb $Serial shell settings put global $setting 0 | Out-Null
    }
    Invoke-Adb $Serial shell svc power stayon true | Out-Null
    Invoke-Adb $Serial shell input keyevent KEYCODE_WAKEUP | Out-Null
    Invoke-Adb $Serial shell wm dismiss-keyguard | Out-Null

    $builds = if ($BuildType -eq 'both') { @('debug', 'staging') } else { @($BuildType) }
    $summaries = New-Object System.Collections.Generic.List[string]
    for ($round = 1; $round -le $Repeat; $round++) {
        foreach ($build in $builds) {
            Write-Host "=== $build (round $round of $Repeat) ==="
            New-Item -ItemType Directory -Force -Path $ResultsDir | Out-Null
            $gradleLog = Join-Path $ResultsDir "$build-round$round-gradle.log"
            $variantResults = Join-Path $androidDir "e2e\build\outputs\androidTest-results\connected\$build"
            # Gradle leaves the last run's results in place when it fails before running the
            # tests (a compile error): remove them, so they are never counted for this round.
            if (Test-Path -LiteralPath $variantResults) { Remove-Item -LiteralPath $variantResults -Recurse -Force }
            $code = Invoke-GradleRun -Build $build -Ready $ready -ControlPort $controlPort -LogFile $gradleLog
            $kept = Join-Path $ResultsDir "$build-round$round"
            if (Test-Path -LiteralPath $kept) { Remove-Item -LiteralPath $kept -Recurse -Force }
            if (Test-Path -LiteralPath $variantResults) {
                New-Item -ItemType Directory -Force -Path $kept | Out-Null
                Copy-Item -Path (Join-Path $variantResults '*') -Destination $kept -Recurse -Force
            }
            $summary = "$build round ${round}: gradle exit $code, $(Get-RunSummary -Dir $kept)"
            Write-Host $summary
            $summaries.Add($summary)
            if ($code -ne 0) { $exitCode = 1 }
        }
    }
    Write-Host '=== summary ==='
    $summaries | ForEach-Object { Write-Host $_ }
    Write-Host "results: $ResultsDir"
} catch {
    Write-Error "$_" -ErrorAction Continue
    $exitCode = 1
} finally {
    if ($null -ne $listener) { $listener.Stop() }
    foreach ($rule in $reversed) {
        try { Invoke-Adb $Serial reverse --remove $rule | Out-Null } catch { Write-Warning "could not remove reverse $rule`: $_" }
    }
    if ($null -ne $server) {
        if (-not $server.HasExited) {
            try {
                $server.StandardInput.Write("quit`n")
                $server.StandardInput.Close()
            } catch {
                Write-Warning "could not ask the test server to quit: $_"
            }
            if (-not $server.WaitForExit(30000)) {
                Write-Warning 'the test server did not quit within 30 s; killing it'
                & taskkill.exe /T /F /PID $server.Id | Out-Null
                $server.WaitForExit()
            }
        }
        Write-Host "test server exited with $($server.ExitCode)"
        if ($server.ExitCode -ne 0) { $exitCode = 1 }
    }
    if (-not $KeepState) {
        Remove-Item -LiteralPath $stateDir -Recurse -Force -ErrorAction SilentlyContinue
        if (Test-Path -LiteralPath $stateDir) { Write-Warning "could not delete $stateDir" }
    } else {
        Write-Host "state kept in $stateDir"
    }
}
exit $exitCode
