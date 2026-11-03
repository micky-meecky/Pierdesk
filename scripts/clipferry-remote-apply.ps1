[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$StagedExe,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9A-Fa-f]{64}$')]
    [string]$ExpectedSha256,

    [Parameter(Mandatory = $true)]
    [string]$StatusPath,

    [Parameter(Mandatory = $true)]
    [string]$ExpectedHostname,

    [switch]$Force,
    [string]$TargetExe
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$ExpectedSha256 = $ExpectedSha256.ToUpperInvariant()
$sessionId = [System.Diagnostics.Process]::GetCurrentProcess().SessionId
$targetPath = $null
$backupPath = $null
$oldSha256 = $null
$replacementStarted = $false
$forcedTermination = $false
$startedProcessId = $null
$stopAttempted = $false
$oldProcessIds = @()
$oldVersion = $null
$newVersion = $null
$updateMutex = $null
$mutexOwned = $false

function Initialize-TrayProbe {
    if ('PierdeskUpdateTrayProbe' -as [type]) { return }
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class PierdeskUpdateTrayProbe {
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern IntPtr FindWindowEx(IntPtr parent, IntPtr after, string cls, string title);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr window, out uint pid);
    [DllImport("user32.dll", SetLastError=true)] static extern IntPtr SendMessageTimeout(IntPtr window, uint msg, UIntPtr wp, IntPtr lp, uint flags, uint timeout, out UIntPtr result);
    public static bool Ready(int pid) {
        IntPtr window=FindWindowEx(new IntPtr(-3), IntPtr.Zero, "PierdeskTrayWindow", null);
        uint owner; GetWindowThreadProcessId(window, out owner);
        UIntPtr result;
        return window!=IntPtr.Zero && owner==(uint)pid && SendMessageTimeout(window, 0, UIntPtr.Zero, IntPtr.Zero, 2, 1000, out result)!=IntPtr.Zero;
    }
}
'@
}

function Test-TrayReceipt {
    param($Receipt, [int]$ProcessId, [string]$Path, [string]$Hash, [string]$Version, [bool]$RequireElevated)
    return ($Receipt.processId -eq $ProcessId -and $Receipt.executablePath -ieq $Path -and
        $Receipt.sha256 -ieq $Hash -and [version]($Receipt.version + '.0') -eq [version]$Version -and
        (-not $RequireElevated -or $Receipt.elevated -eq $true))
}

function Test-ReadyProcess {
    param([int]$ProcessId, [string]$Path)
    try {
        $candidate = Get-CimInstance Win32_Process -Filter "ProcessId=$ProcessId" -ErrorAction Stop
        if (-not $candidate -or $candidate.ExecutablePath -ine $Path -or [int]$candidate.SessionId -ne $sessionId) { return $false }
        if (-not [PierdeskUpdateTrayProbe]::Ready($ProcessId)) { return $false }
        $version = (Get-Item -LiteralPath $Path).VersionInfo.FileVersion
        # Legacy rollback binaries predate startup receipts, but must still own a responsive tray.
        if ([version]$version -lt [version]'1.0.1') { return $true }
        $receiptPath = Join-Path $env:LOCALAPPDATA "ClipFerry\runtime\tray-$ProcessId.json"
        $receiptFile = Get-Item -LiteralPath $receiptPath -ErrorAction Stop
        if ($receiptFile.LastWriteTimeUtc -lt $candidate.CreationDate.ToUniversalTime()) { return $false }
        $receipt = [IO.File]::ReadAllText($receiptPath) | ConvertFrom-Json
        $hash = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
        return (Test-TrayReceipt $receipt $ProcessId $Path $hash $version $true)
    } catch { return $false }
}

function Write-DeployStatus {
    param(
        [Parameter(Mandatory = $true)]
        [hashtable]$Values
    )

    $parent = Split-Path -Parent $StatusPath
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
    $temporary = "$StatusPath.tmp"
    $payload = [ordered]@{
        schemaVersion = 2
        time = (Get-Date).ToString('o')
        hostname = $env:COMPUTERNAME
        user = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name
        sessionId = $sessionId
        oldProcessIds = $oldProcessIds
        oldVersion = $oldVersion
        newVersion = $newVersion
    }
    foreach ($entry in $Values.GetEnumerator()) {
        $payload[$entry.Key] = $entry.Value
    }
    $json = $payload | ConvertTo-Json -Depth 5
    $json | Set-Content -LiteralPath $temporary -Encoding UTF8
    if ($Values.state -in @('succeeded', 'failed')) {
        $receipts = Join-Path $env:LOCALAPPDATA 'ClipFerry\update-history'
        New-Item -ItemType Directory -Force -Path $receipts | Out-Null
        $name = (Split-Path -Leaf $parent) + '.json'
        $json | Set-Content -LiteralPath (Join-Path $receipts $name) -Encoding UTF8
    }
    # Publish completion only after its durable receipt exists.
    Move-Item -LiteralPath $temporary -Destination $StatusPath -Force
}

function Get-InteractivePierdeskProcesses {
    Get-CimInstance Win32_Process -Filter "Name='pierdesk.exe'" |
        Where-Object { [int]$_.SessionId -eq $sessionId }
}

# One-time migration from the former product name. Stop a running clipferry.exe with its own
# tray-exit, then give its file the new name so the normal flow can replace it in place.
function Move-LegacyInstallation {
    $legacyFilter = "Name='clipferry.exe'"
    $legacyPaths = @(Get-CimInstance Win32_Process -Filter $legacyFilter |
        Where-Object { [int]$_.SessionId -eq $sessionId -and $_.ExecutablePath } |
        ForEach-Object { $_.ExecutablePath } | Sort-Object -Unique)
    foreach ($legacyPath in $legacyPaths) {
        $stop = [Diagnostics.ProcessStartInfo]::new($legacyPath, 'tray-exit')
        $stop.UseShellExecute = $false
        $stop.CreateNoWindow = $true
        $request = [Diagnostics.Process]::Start($stop)
        if (-not $request.WaitForExit(5000)) { $request.Kill(); throw 'Timed out asking the previous ClipFerry to exit.' }
        $request.Dispose()
    }
    if ($legacyPaths.Count -gt 0) {
        $deadline = (Get-Date).AddSeconds(12)
        do {
            Start-Sleep -Milliseconds 200
            $remaining = @(Get-CimInstance Win32_Process -Filter $legacyFilter |
                Where-Object { [int]$_.SessionId -eq $sessionId })
        } until ($remaining.Count -eq 0 -or (Get-Date) -ge $deadline)
        if ($remaining.Count -gt 0) {
            $script:forcedTermination = $true
            $remaining | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
            Wait-Process -Id @($remaining.ProcessId) -Timeout 5 -ErrorAction SilentlyContinue
        }
    }
    $desktopLegacy = Join-Path ([Environment]::GetFolderPath('Desktop')) 'clipferry.exe'
    foreach ($candidate in @($legacyPaths) + $desktopLegacy | Sort-Object -Unique) {
        if ((Split-Path -Leaf $candidate) -ine 'clipferry.exe' -or -not (Test-Path -LiteralPath $candidate -PathType Leaf)) { continue }
        $renamed = Join-Path (Split-Path -Parent $candidate) 'pierdesk.exe'
        if (Test-Path -LiteralPath $renamed) { continue }
        Move-Item -LiteralPath $candidate -Destination $renamed
        if (-not (Get-Variable -Name migratedTarget -Scope Script -ValueOnly -ErrorAction SilentlyContinue)) { $script:migratedTarget = $renamed }
    }
}

function Find-InstalledPierdeskPath {
    $migrated = Get-Variable -Name migratedTarget -Scope Script -ValueOnly -ErrorAction SilentlyContinue
    if ($migrated -and -not $TargetExe) { return $migrated }
    $running = @(Get-InteractivePierdeskProcesses)
    $runningPaths = @($running |
        ForEach-Object { $_.ExecutablePath } |
        Where-Object { $_ -and (Split-Path -Leaf $_) -ieq 'pierdesk.exe' } |
        Sort-Object -Unique)
    if ($runningPaths.Count -gt 1) { throw 'Multiple Pierdesk executable paths are running; close extra copies before updating.' }
    if ($TargetExe) {
        $target = [IO.Path]::GetFullPath($TargetExe)
        if ($runningPaths.Count -eq 1 -and $runningPaths[0] -ine $target) {
            throw "Pierdesk is running from a different path: $($runningPaths[0]). Close it before updating $target."
        }
        return $target
    }
    if ($runningPaths.Count -eq 1) {
        return [System.IO.Path]::GetFullPath($runningPaths[0])
    }

    $runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
    if (Test-Path -LiteralPath $runKey) {
        $properties = Get-ItemProperty -LiteralPath $runKey
        foreach ($property in $properties.PSObject.Properties) {
            if ($property.Name -like 'PS*' -or $property.Value -isnot [string]) {
                continue
            }
            $value = [string]$property.Value
            $candidate = $null
            if ($value -match '^\s*"([^"]+pierdesk\.exe)"') {
                $candidate = $Matches[1]
            }
            elseif ($value -match '^\s*(.+?pierdesk\.exe)(?:\s|$)') {
                $candidate = $Matches[1]
            }
            if ($candidate -and (Test-Path -LiteralPath $candidate -PathType Leaf)) {
                return [System.IO.Path]::GetFullPath($candidate)
            }
        }
    }

    $desktopCandidate = Join-Path ([Environment]::GetFolderPath('Desktop')) 'pierdesk.exe'
    if (Test-Path -LiteralPath $desktopCandidate -PathType Leaf) {
        return [System.IO.Path]::GetFullPath($desktopCandidate)
    }

    throw 'Unable to locate the installed pierdesk.exe in the interactive desktop session.'
}

function Stop-InteractivePierdesk {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ExecutablePath
    )

    $processes = @(Get-InteractivePierdeskProcesses | Where-Object {
        $_.ExecutablePath -and $_.ExecutablePath -ieq $ExecutablePath
    })
    if ($processes.Count -eq 0) {
        return
    }

    $stop = [Diagnostics.ProcessStartInfo]::new($ExecutablePath, 'tray-exit')
    $stop.UseShellExecute = $false
    $stop.CreateNoWindow = $true
    $request = [Diagnostics.Process]::Start($stop)
    if (-not $request.WaitForExit(5000)) { $request.Kill(); throw 'Timed out requesting tray exit.' }
    $request.Dispose()
    $deadline = (Get-Date).AddSeconds(12)
    do {
        Start-Sleep -Milliseconds 200
        $remaining = @(Get-InteractivePierdeskProcesses | Where-Object {
            $_.ExecutablePath -and $_.ExecutablePath -ieq $ExecutablePath
        })
    } until ($remaining.Count -eq 0 -or (Get-Date) -ge $deadline)

    if ($remaining.Count -gt 0) {
        $script:forcedTermination = $true
        $remaining | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
        Wait-Process -Id @($remaining.ProcessId) -Timeout 5 -ErrorAction SilentlyContinue
    }

    $remaining = @(Get-InteractivePierdeskProcesses | Where-Object {
        $_.ExecutablePath -and $_.ExecutablePath -ieq $ExecutablePath
    })
    if ($remaining.Count -gt 0) {
        throw 'The existing Pierdesk process did not exit.'
    }
}

function Start-InteractivePierdesk {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ExecutablePath
    )

    $process = Start-Process -FilePath $ExecutablePath -ArgumentList 'tray' -WindowStyle Hidden -PassThru
    $null = $process.Handle
    $deadline = (Get-Date).AddSeconds(30)
    $readySince = $null
    do {
        Start-Sleep -Milliseconds 250
        if ($process.HasExited) { throw 'The exact updated process exited before readiness (another instance may still own the tray).' }
        if (Test-ReadyProcess -ProcessId $process.Id -Path $ExecutablePath) {
            if (-not $readySince) { $readySince = Get-Date }
            if (((Get-Date) - $readySince).TotalSeconds -ge 2) { return [int]$process.Id }
        } else { $readySince = $null }
    } until ((Get-Date) -ge $deadline)
    throw 'Updated process did not pass PID/session/image/version/elevation/tray-response verification.'
}

try {
    $updateMutex = [Threading.Mutex]::new($false, 'Local\ClipFerry.Update.v1')
    $mutexOwned = $updateMutex.WaitOne(0)
    if (-not $mutexOwned) { throw 'Another Pierdesk update is already in progress.' }
    Initialize-TrayProbe
    Write-DeployStatus -Values @{ state = 'running'; phase = 'preflight' }

    if ($env:COMPUTERNAME -ine $ExpectedHostname) {
        throw "Host identity mismatch: expected '$ExpectedHostname', got '$env:COMPUTERNAME'."
    }
    if (-not (Test-Path -LiteralPath $StagedExe -PathType Leaf)) {
        throw "Staged executable was not found: $StagedExe"
    }
    if ((Split-Path -Leaf $StagedExe) -ine 'incoming.exe') {
        throw "Unexpected staged executable name: $StagedExe"
    }
    $stagedSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $StagedExe).Hash.ToUpperInvariant()
    if ($stagedSha256 -ne $ExpectedSha256) {
        throw "Staged executable checksum mismatch: expected $ExpectedSha256, got $stagedSha256."
    }

    Move-LegacyInstallation
    $targetPath = Find-InstalledPierdeskPath
    if ((Split-Path -Leaf $targetPath) -ine 'pierdesk.exe') {
        throw "Refusing to replace an unexpected target: $targetPath"
    }
    if (-not (Test-Path -LiteralPath $targetPath -PathType Leaf)) {
        throw "Installed executable was not found: $targetPath"
    }

    $oldSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $targetPath).Hash.ToUpperInvariant()
    $oldVersion = (Get-Item -LiteralPath $targetPath).VersionInfo.FileVersion
    $newVersion = (Get-Item -LiteralPath $StagedExe).VersionInfo.FileVersion
    if (-not $Force -and $oldSha256 -ne $ExpectedSha256 -and [version]$newVersion -le [version]$oldVersion) {
        throw "Refusing a different binary with non-increasing version ($oldVersion -> $newVersion). Package a new version, or explicitly force an intended rollback."
    }
    $oldProcessIds = @(Get-InteractivePierdeskProcesses | ForEach-Object { [int]$_.ProcessId })
    if ($oldSha256 -eq $ExpectedSha256 -and -not $Force) {
        $existingProcess = @(Get-InteractivePierdeskProcesses | Where-Object {
            $_.ExecutablePath -and $_.ExecutablePath -ieq $targetPath
        } | Select-Object -First 1)
        if ($existingProcess.Count -gt 0 -and (Test-ReadyProcess -ProcessId $existingProcess[0].ProcessId -Path $targetPath)) {
          Write-DeployStatus -Values @{
            state = 'succeeded'
            outcome = 'already_current'
            targetPath = $targetPath
            oldSha256 = $oldSha256
            newSha256 = $ExpectedSha256
            processId = if ($existingProcess.Count -gt 0) { [int]$existingProcess[0].ProcessId } else { $null }
            forcedTermination = $false
        }
          exit 0
        }
        # Same bytes need only a verified restart. In particular, local recovery
        # of a missing target must not overwrite an older usable rollback copy.
        $stopAttempted = $true
        Stop-InteractivePierdesk -ExecutablePath $targetPath
        $startedProcessId = Start-InteractivePierdesk -ExecutablePath $targetPath
        Write-DeployStatus -Values @{
            state = 'succeeded'; outcome = 'restarted_current'; targetPath = $targetPath
            oldSha256 = $oldSha256; newSha256 = $ExpectedSha256
            processId = $startedProcessId; forcedTermination = $forcedTermination
        }
        exit 0
    }

    Write-DeployStatus -Values @{
        state = 'running'
        phase = 'stopping'
        targetPath = $targetPath
        oldSha256 = $oldSha256
        newSha256 = $ExpectedSha256
    }
    $stopAttempted = $true
    Stop-InteractivePierdesk -ExecutablePath $targetPath

    $targetDirectory = Split-Path -Parent $targetPath
    $temporaryPath = Join-Path $targetDirectory '.pierdesk.update.exe'
    $backupPath = Join-Path $targetDirectory 'pierdesk.previous.exe'
    Remove-Item -LiteralPath $temporaryPath -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $backupPath -Force -ErrorAction SilentlyContinue
    Copy-Item -LiteralPath $StagedExe -Destination $temporaryPath -Force
    $temporarySha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $temporaryPath).Hash.ToUpperInvariant()
    if ($temporarySha256 -ne $ExpectedSha256) {
        throw "Local replacement copy checksum mismatch: expected $ExpectedSha256, got $temporarySha256."
    }

    Write-DeployStatus -Values @{
        state = 'running'
        phase = 'replacing'
        targetPath = $targetPath
        oldSha256 = $oldSha256
        newSha256 = $ExpectedSha256
        forcedTermination = $forcedTermination
    }
    [System.IO.File]::Replace($temporaryPath, $targetPath, $backupPath, $true)
    $replacementStarted = $true

    $installedSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $targetPath).Hash.ToUpperInvariant()
    if ($installedSha256 -ne $ExpectedSha256) {
        throw "Installed executable checksum mismatch: expected $ExpectedSha256, got $installedSha256."
    }

    $startedProcessId = Start-InteractivePierdesk -ExecutablePath $targetPath
    Write-DeployStatus -Values @{
        state = 'succeeded'
        outcome = 'updated'
        targetPath = $targetPath
        oldSha256 = $oldSha256
        newSha256 = $installedSha256
        backupPath = $backupPath
        processId = $startedProcessId
        forcedTermination = $forcedTermination
    }
    exit 0
}
catch {
    $failure = $_.Exception.Message
    $rolledBack = $false
    $rollbackError = $null

    if ($replacementStarted -and $targetPath -and $backupPath -and (Test-Path -LiteralPath $backupPath -PathType Leaf)) {
        try {
            Stop-InteractivePierdesk -ExecutablePath $targetPath
            Remove-Item -LiteralPath $targetPath -Force -ErrorAction SilentlyContinue
            Move-Item -LiteralPath $backupPath -Destination $targetPath -Force
            $startedProcessId = Start-InteractivePierdesk -ExecutablePath $targetPath
            $rolledBack = $true
        }
        catch {
            $rollbackError = $_.Exception.Message
        }
    }
    elseif ($stopAttempted -and $targetPath -and (Test-Path -LiteralPath $targetPath -PathType Leaf)) {
        try {
            $running = @(Get-InteractivePierdeskProcesses | Where-Object {
                $_.ExecutablePath -and $_.ExecutablePath -ieq $targetPath
            })
            if ($running.Count -eq 0) {
                $startedProcessId = Start-InteractivePierdesk -ExecutablePath $targetPath
            }
        }
        catch {
            $rollbackError = $_.Exception.Message
        }
    }

    Write-DeployStatus -Values @{
        state = 'failed'
        outcome = if ($rolledBack) { 'rolled_back' } else { 'failed' }
        targetPath = $targetPath
        oldSha256 = $oldSha256
        expectedSha256 = $ExpectedSha256
        error = $failure
        rollbackError = $rollbackError
        processId = $startedProcessId
        forcedTermination = $forcedTermination
    }
    exit 1
}
finally {
    if ($mutexOwned) { $updateMutex.ReleaseMutex() }
    if ($updateMutex) { $updateMutex.Dispose() }
}
