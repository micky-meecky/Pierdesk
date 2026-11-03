[CmdletBinding()]
param(
    [string]$ExePath,
    [switch]$Elevated,
    [string]$ResultPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\local-update-support.ps1"

if ($ResultPath) {
    $ResultPath = [IO.Path]::GetFullPath($ResultPath)
    $resultRoot = Join-Path $env:LOCALAPPDATA 'ClipFerry\update-results'
    if ((Split-Path -Parent $ResultPath) -ine $resultRoot -or (Split-Path -Leaf $ResultPath) -notmatch '^[a-zA-Z0-9]+\.tmp$') {
        throw 'Unexpected updater result path.'
    }
}
trap {
    # The elevated invocation writes the underlying error first. Do not replace it
    # with the parent's less useful "exit code 1" message.
    if ($ResultPath -and -not (Test-Path -LiteralPath $ResultPath)) {
        $message = $_.Exception.Message
        if ($_.Exception -is [ComponentModel.Win32Exception] -and $_.Exception.NativeErrorCode -eq 1223) { $message = 'Windows UAC confirmation was cancelled. No update was performed.' }
        [IO.Directory]::CreateDirectory((Split-Path -Parent $ResultPath)) | Out-Null
        [IO.File]::WriteAllText($ResultPath, $message, [Text.Encoding]::Unicode)
    }
    Write-Error $_ -ErrorAction Continue
    exit 1
}

if ([string]::IsNullOrWhiteSpace($ExePath)) {
    $ExePath = Join-Path $PSScriptRoot '..\dist\pierdesk.exe'
}

function Test-IsAdministrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]$identity
    $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Quote-ProcessArgument {
    param([Parameter(Mandatory = $true)][string]$Value)
    '"' + $Value.Replace('"', '\"') + '"'
}

$ExePath = [System.IO.Path]::GetFullPath($ExePath)
if (-not (Test-Path -LiteralPath $ExePath -PathType Leaf)) {
    throw "Executable was not found: $ExePath"
}
if ((Split-Path -Leaf $ExePath) -cne 'pierdesk.exe') {
    throw "The deployment artifact must be named exactly pierdesk.exe: $ExePath"
}

if (-not (Test-IsAdministrator)) {
    if ($Elevated) {
        throw 'The local updater was relaunched without administrator rights.'
    }
    $arguments = @(
        '-NoProfile',
        '-NonInteractive',
        '-WindowStyle', 'Hidden',
        '-ExecutionPolicy', 'Bypass',
        '-File', (Quote-ProcessArgument -Value $PSCommandPath),
        '-ExePath', (Quote-ProcessArgument -Value $ExePath),
        '-Elevated'
    )
    if ($ResultPath) { $arguments += @('-ResultPath', (Quote-ProcessArgument $ResultPath)) }
    Write-Host 'Windows will request administrator confirmation once.' -ForegroundColor Cyan
    $process = Start-Process -FilePath "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -Verb RunAs -WindowStyle Hidden -ArgumentList $arguments -PassThru
    # Wait for the updater itself, not its long-lived Pierdesk descendant.
    # Cache the handle so Windows PowerShell 5.1 retains the exit code.
    $null = $process.Handle
    $process.WaitForExit()
    if ($process.ExitCode -ne 0) {
        throw "The elevated local updater failed with exit code $($process.ExitCode)."
    }
    Write-Host 'Local Pierdesk update completed.' -ForegroundColor Green
    exit 0
}

$sourceSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $ExePath).Hash.ToUpperInvariant()
$token = [Guid]::NewGuid().ToString('N')
$updateRoot = [System.IO.Path]::GetFullPath((Join-Path $env:LOCALAPPDATA 'ClipFerry\updates'))
$updateDirectory = [System.IO.Path]::GetFullPath((Join-Path $updateRoot $token))
if ((Split-Path -Parent $updateDirectory) -ine $updateRoot -or (Split-Path -Leaf $updateDirectory) -notmatch '^[0-9a-f]{32}$') {
    throw "Refusing to use an unexpected update directory: $updateDirectory"
}

New-Item -ItemType Directory -Force -Path $updateDirectory | Out-Null
$stagedExe = Join-Path $updateDirectory 'incoming.exe'
$applyScript = Join-Path $updateDirectory 'remote-apply.ps1'
$statusPath = Join-Path $updateDirectory 'status.json'
$sourceApplyScript = Join-Path $PSScriptRoot 'clipferry-remote-apply.ps1'
$completed = $false
$stdoutPath = Join-Path $updateDirectory 'helper.stdout.log'
$stderrPath = Join-Path $updateDirectory 'helper.stderr.log'

try {
    Copy-Item -LiteralPath $ExePath -Destination $stagedExe -Force
    Copy-Item -LiteralPath $sourceApplyScript -Destination $applyScript -Force
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $stagedExe).Hash.ToUpperInvariant() -ne $sourceSha256) {
        throw 'The local staged executable checksum does not match the release artifact.'
    }

    # Always update the installed desktop path, never an accidentally running dist copy.
    $targetExe = Join-Path ([Environment]::GetFolderPath('Desktop')) 'pierdesk.exe'
    $localSession = [Diagnostics.Process]::GetCurrentProcess().SessionId
    $otherCopies = @(Get-CimInstance Win32_Process -Filter "Name='pierdesk.exe'" | Where-Object {
        $_.SessionId -eq $localSession -and $_.ExecutablePath -and $_.ExecutablePath -ine $targetExe
    })
    if ($otherCopies.Count -gt 0) {
        throw "Close Pierdesk from its tray before updating. It is running outside the installed path: $($otherCopies[0].ExecutablePath). Normal startup uses: $targetExe"
    }
    # Recover a missing local installation from the verified staged release. This
    # is deliberately local-only; remote deployment still requires an existing target.
    $restored = Initialize-LocalPierdeskTarget $targetExe $stagedExe $sourceSha256
    if ($restored) {
        $history = Join-Path $env:LOCALAPPDATA 'ClipFerry\update-history'
        New-Item -ItemType Directory -Force -Path $history | Out-Null
        @{time=(Get-Date).ToString('o');targetPath=$targetExe;sha256=$sourceSha256;outcome='missing_target_restored';startupVerified=$false} |
            ConvertTo-Json | Set-Content -LiteralPath (Join-Path $history "$token.bootstrap.json") -Encoding UTF8
    }
    $arguments = @('update-apply', (Quote-ProcessArgument $stagedExe), $sourceSha256,
        (Quote-ProcessArgument $statusPath), $env:COMPUTERNAME, (Quote-ProcessArgument $targetExe))
    $process = Start-Process -FilePath $stagedExe -WindowStyle Hidden -ArgumentList $arguments -PassThru
    $null = $process.Handle
    $process.WaitForExit()
    if (-not (Test-Path -LiteralPath $statusPath -PathType Leaf)) {
        throw "The local update helper returned no status (exit code $($process.ExitCode)). Diagnostic files were preserved at: $updateDirectory"
    }
    $status = [System.IO.File]::ReadAllText($statusPath) | ConvertFrom-Json
    if ($status.state -eq 'failed') {
        $rollback = if ($status.PSObject.Properties['rollbackError']) { $status.rollbackError } else { 'not started' }
        throw "Local update failed: $($status.error); rollback: $rollback. Diagnostic files: $updateDirectory"
    }
    if ($process.ExitCode -ne 0 -or $status.state -ne 'succeeded') {
        throw "Local update did not finish (exit code $($process.ExitCode), state $($status.state)). Diagnostic files: $updateDirectory"
    }
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $status.targetPath).Hash.ToUpperInvariant() -ne $sourceSha256) {
        throw 'The installed local executable checksum does not match the release artifact.'
    }

    Write-Host "Local Pierdesk is current: $($status.targetPath)" -ForegroundColor Green
    Write-Host "SHA-256: $sourceSha256" -ForegroundColor Green
    & "$PSScriptRoot\install-update-shortcut.ps1"
    $completed = $true
}
finally {
    if ($completed -and (Test-Path -LiteralPath $updateDirectory -PathType Container)) {
        Remove-Item -LiteralPath $updateDirectory -Recurse -Force -ErrorAction SilentlyContinue
    }
}
