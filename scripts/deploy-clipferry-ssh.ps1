[CmdletBinding()]
param(
    [string]$ExePath,
    [string]$TargetsPath,
    [int]$TimeoutSeconds = 120,
    [switch]$Force,
    [switch]$ValidateOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($ExePath)) {
    $ExePath = Join-Path $PSScriptRoot '..\dist\pierdesk.exe'
}
if ([string]::IsNullOrWhiteSpace($TargetsPath)) {
    $TargetsPath = Join-Path $PSScriptRoot 'clipferry-deploy-targets.local.psd1'
}

function ConvertTo-SingleQuotedLiteral {
    param([Parameter(Mandatory = $true)][string]$Value)
    "'" + $Value.Replace("'", "''") + "'"
}

function Invoke-PierdeskNative {
    param(
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [Parameter(Mandatory = $true)][string]$ConfigurationId
    )

    $oldAskPass = $env:SSH_ASKPASS
    $oldAskPassRequire = $env:SSH_ASKPASS_REQUIRE
    $oldDisplay = $env:DISPLAY
    $oldAskPassId = $env:PIERDESK_SSH_ASKPASS_ID
    try {
        $env:SSH_ASKPASS = $script:AskPassPath
        $env:SSH_ASKPASS_REQUIRE = 'force'
        $env:DISPLAY = 'pierdesk'
        $env:PIERDESK_SSH_ASKPASS_ID = $ConfigurationId
        $output = @(& $Executable @Arguments 2>&1 | ForEach-Object { "$_" })
        $exitCode = $LASTEXITCODE
    }
    finally {
        $env:SSH_ASKPASS = $oldAskPass
        $env:SSH_ASKPASS_REQUIRE = $oldAskPassRequire
        $env:DISPLAY = $oldDisplay
        $env:PIERDESK_SSH_ASKPASS_ID = $oldAskPassId
    }

    if ($exitCode -ne 0) {
        $details = ($output -join [Environment]::NewLine).Trim()
        throw "Remote command failed with exit code $exitCode.$([Environment]::NewLine)$details"
    }
    return $output
}

function Invoke-RemotePowerShell {
    param(
        [Parameter(Mandatory = $true)][hashtable]$Target,
        [Parameter(Mandatory = $true)][string]$Script
    )

    $utf8Preamble = @'
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$OutputEncoding = [System.Text.UTF8Encoding]::new($false)
'@
    $scriptName = 'pierdesk-run-' + [Guid]::NewGuid().ToString('N') + '.ps1'
    $localScript = Join-Path ([IO.Path]::GetTempPath()) $scriptName
    $remoteScript = 'AppData/Local/Temp/' + $scriptName
    $body = $utf8Preamble + "`r`ntry {`r`n" + $Script + "`r`n} finally { Remove-Item -LiteralPath `$PSCommandPath -Force -ErrorAction SilentlyContinue }`r`n"
    [IO.File]::WriteAllText($localScript, $body, [Text.UTF8Encoding]::new($true))
    try {
        Copy-ToRemote -Target $Target -LocalPath $localScript -RemoteRelativePath $remoteScript
        Invoke-PierdeskNative -Executable $script:SshPath -ConfigurationId $Target.ConfigurationId -Arguments @(
            '-n', '-F', $Target.ConfigurationPath,
            $Target.Alias,
            'powershell.exe', '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', $remoteScript
        )
    } finally { Remove-Item -LiteralPath $localScript -Force -ErrorAction SilentlyContinue }
}

function Copy-ToRemote {
    param(
        [Parameter(Mandatory = $true)][hashtable]$Target,
        [Parameter(Mandatory = $true)][string]$LocalPath,
        [Parameter(Mandatory = $true)][string]$RemoteRelativePath
    )

    Invoke-PierdeskNative -Executable $script:ScpPath -ConfigurationId $Target.ConfigurationId -Arguments @(
        '-q', '-F', $Target.ConfigurationPath,
        $LocalPath,
        "$($Target.Alias):$RemoteRelativePath"
    ) | Out-Null
}

if ($TimeoutSeconds -lt 30 -or $TimeoutSeconds -gt 900) {
    throw 'TimeoutSeconds must be between 30 and 900.'
}

$ExePath = [System.IO.Path]::GetFullPath($ExePath)
$TargetsPath = [System.IO.Path]::GetFullPath($TargetsPath)
if (-not (Test-Path -LiteralPath $ExePath -PathType Leaf)) {
    throw "Executable was not found: $ExePath"
}
if ((Split-Path -Leaf $ExePath) -cne 'pierdesk.exe') {
    throw "The deployment artifact must be named exactly pierdesk.exe: $ExePath"
}
if (-not (Test-Path -LiteralPath $TargetsPath -PathType Leaf)) {
    throw "Deployment target file was not found: $TargetsPath"
}

$remoteApplyPath = Join-Path $PSScriptRoot 'clipferry-remote-apply.ps1'
if (-not (Test-Path -LiteralPath $remoteApplyPath -PathType Leaf)) {
    throw "Remote update helper was not found: $remoteApplyPath"
}

$sourceSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $ExePath).Hash.ToUpperInvariant()
$sshRoot = Join-Path $env:LOCALAPPDATA 'ClipFerry\ssh'
$script:AskPassPath = Join-Path $sshRoot 'pierdesk-ssh-askpass.exe'
$runtimeRoot = Join-Path $sshRoot 'runtime\10.0.0.0p2'
$script:SshPath = Join-Path $runtimeRoot 'ssh.exe'
$script:ScpPath = Join-Path $runtimeRoot 'scp.exe'
foreach ($requiredPath in @($script:AskPassPath, $script:SshPath, $script:ScpPath)) {
    if (-not (Test-Path -LiteralPath $requiredPath -PathType Leaf)) {
        throw "Required Pierdesk SSH component was not found: $requiredPath"
    }
}

$targetData = Import-PowerShellDataFile -LiteralPath $TargetsPath
if (-not $targetData.ContainsKey('Targets') -or @($targetData.Targets).Count -eq 0) {
    throw "No targets were defined in: $TargetsPath"
}

$targets = foreach ($entry in @($targetData.Targets)) {
    foreach ($field in @('Name', 'ExpectedHostname', 'ConfigurationId')) {
        if (-not $entry.ContainsKey($field) -or [string]::IsNullOrWhiteSpace([string]$entry[$field])) {
            throw "Every deployment target must define $field."
        }
    }
    if ([string]$entry.ConfigurationId -notmatch '^[0-9a-f]{32}$') {
        throw "Invalid Pierdesk SSH configuration id for target '$($entry.Name)'."
    }
    $configurationPath = Join-Path $sshRoot "$($entry.ConfigurationId).config"
    if (-not (Test-Path -LiteralPath $configurationPath -PathType Leaf)) {
        throw "SSH configuration was not found for target '$($entry.Name)': $configurationPath"
    }
    @{
        Name = [string]$entry.Name
        ExpectedHostname = [string]$entry.ExpectedHostname
        ConfigurationId = [string]$entry.ConfigurationId
        ConfigurationPath = $configurationPath
        Alias = "cf-$($entry.ConfigurationId)"
    }
}

$results = [System.Collections.Generic.List[object]]::new()
foreach ($target in $targets) {
    Write-Host "[$($target.Name)] Checking SSH identity..." -ForegroundColor Cyan
    $expectedHostnameLiteral = ConvertTo-SingleQuotedLiteral -Value $target.ExpectedHostname
    $preflightScript = @"
`$ErrorActionPreference = 'Stop'
`$expectedHostname = $expectedHostnameLiteral
if (`$env:COMPUTERNAME -ine `$expectedHostname) {
    throw "Host identity mismatch: expected '`$expectedHostname', got '`$env:COMPUTERNAME'."
}
`$interactiveUser = (Get-CimInstance Win32_ComputerSystem).UserName
if ([string]::IsNullOrWhiteSpace(`$interactiveUser)) {
    throw 'No interactive Windows user is signed in.'
}
[ordered]@{
    hostname = `$env:COMPUTERNAME
    interactiveUser = `$interactiveUser
    sshService = (Get-Service -Name 'ClipFerrySshd' -ErrorAction Stop).Status.ToString()
} | ConvertTo-Json -Compress
"@
    try {
        $preflightOutput = Invoke-RemotePowerShell -Target $target -Script $preflightScript
        $preflightJson = ($preflightOutput | Where-Object { $_ -match '^\s*\{' } | Select-Object -Last 1)
        if (-not $preflightJson) {
            throw 'Remote identity check returned no structured result.'
        }
        $preflight = $preflightJson | ConvertFrom-Json
        if ($preflight.sshService -ne 'Running') {
            throw "Pierdesk SSH service is not running on $($target.Name)."
        }

        if ($ValidateOnly) {
            $results.Add([pscustomobject]@{
                Device = $target.Name
                Hostname = $preflight.hostname
                Result = 'Validated'
                TargetPath = '-'
                Sha256 = $sourceSha256
            })
            continue
        }

        $token = [Guid]::NewGuid().ToString('N')
        $remoteRelativeDirectory = "AppData/Local/ClipFerry/updates/$token"
        $remoteDirectoryScript = @"
`$ErrorActionPreference = 'Stop'
`$directory = Join-Path `$env:LOCALAPPDATA 'ClipFerry\updates\$token'
New-Item -ItemType Directory -Force -Path `$directory | Out-Null
"@
        Invoke-RemotePowerShell -Target $target -Script $remoteDirectoryScript | Out-Null

        Write-Host "[$($target.Name)] Uploading and verifying the update..." -ForegroundColor Cyan
        Copy-ToRemote -Target $target -LocalPath $ExePath -RemoteRelativePath "$remoteRelativeDirectory/incoming.exe"
        Copy-ToRemote -Target $target -LocalPath $remoteApplyPath -RemoteRelativePath "$remoteRelativeDirectory/remote-apply.ps1"

        $forceArgument = if ($Force) { "' --force'" } else { "''" }
        $controllerScript = @"
`$ErrorActionPreference = 'Stop'
`$token = '$token'
`$expectedSha256 = '$sourceSha256'
`$expectedHostname = $expectedHostnameLiteral
`$timeoutSeconds = $TimeoutSeconds
`$directory = Join-Path `$env:LOCALAPPDATA "ClipFerry\updates\`$token"
`$stagedExe = Join-Path `$directory 'incoming.exe'
`$applyScript = Join-Path `$directory 'remote-apply.ps1'
`$statusPath = Join-Path `$directory 'status.json'
`$interactiveUser = (Get-CimInstance Win32_ComputerSystem).UserName
if ([string]::IsNullOrWhiteSpace(`$interactiveUser)) { throw 'No interactive Windows user is signed in.' }

`$taskName = "Pierdesk Update `$token"
`$service = New-Object -ComObject 'Schedule.Service'
`$service.Connect()
`$folder = `$service.GetFolder('\')
`$definition = `$service.NewTask(0)
`$definition.RegistrationInfo.Description = 'Apply a verified Pierdesk update in the interactive desktop session.'
`$definition.Principal.UserId = `$interactiveUser
`$definition.Principal.LogonType = 3
`$definition.Principal.RunLevel = 1
`$definition.Settings.Enabled = `$true
`$definition.Settings.AllowDemandStart = `$true
`$definition.Settings.StartWhenAvailable = `$true
`$definition.Settings.DisallowStartIfOnBatteries = `$false
`$definition.Settings.StopIfGoingOnBatteries = `$false
`$definition.Settings.ExecutionTimeLimit = 'PT5M'
`$action = `$definition.Actions.Create(0)
`$action.Path = `$stagedExe
`$forceArgument = $forceArgument
`$action.Arguments = "update-apply ```"`$stagedExe```" ```"`$expectedSha256```" ```"`$statusPath```" ```"`$expectedHostname```"`$forceArgument"
# Schedule after this SSH command has returned. The updater must survive the gateway exit.
`$trigger = `$definition.Triggers.Create(1)
`$trigger.StartBoundary = (Get-Date).AddSeconds(5).ToString('yyyy-MM-ddTHH:mm:ss')
`$trigger.Enabled = `$true
`$task = `$folder.RegisterTaskDefinition(`$taskName, `$definition, 6, `$null, `$null, 3, `$null)
[ordered]@{state='scheduled';token=`$token;hostname=`$env:COMPUTERNAME} | ConvertTo-Json -Compress
"@

        Write-Host "[$($target.Name)] Applying in the interactive desktop session..." -ForegroundColor Cyan
        # A restart closes the old SSH tunnel. Confirm scheduling first, then use fresh
        # connections to collect the durable receipt; a lost handoff response is not a failure.
        try {
            $controllerOutput = Invoke-RemotePowerShell -Target $target -Script $controllerScript
            $scheduledJson = ($controllerOutput | Where-Object { $_ -match '^\s*\{' } | Select-Object -Last 1)
            if (-not $scheduledJson -or ($scheduledJson | ConvertFrom-Json).token -ne $token) { throw 'Update handoff was not acknowledged.' }
        } catch {
            Write-Host "[$($target.Name)] Update handoff connection ended; checking its durable receipt..." -ForegroundColor Yellow
        }
        $receiptScript = @"
`$ErrorActionPreference = 'Stop'
`$path = Join-Path `$env:LOCALAPPDATA 'ClipFerry\updates\$token\status.json'
if (Test-Path -LiteralPath `$path -PathType Leaf) { [IO.File]::ReadAllText(`$path) | ConvertFrom-Json | ConvertTo-Json -Depth 8 -Compress } else { '{"state":"waiting"}' }
"@
        $status = $null
        $receiptDeadline = (Get-Date).AddSeconds($TimeoutSeconds)
        $lastReceiptError = ''
        do {
            Start-Sleep -Seconds 2
            try {
                $receiptOutput = Invoke-RemotePowerShell -Target $target -Script $receiptScript
                $statusJson = ($receiptOutput | Where-Object { $_ -match '^\s*\{' } | Select-Object -Last 1)
                if ($statusJson) {
                    $candidate = $statusJson | ConvertFrom-Json
                    if ($candidate.state -in @('succeeded','failed')) { $status = $candidate; break }
                }
            } catch { $lastReceiptError = $_.Exception.Message }
        } until ((Get-Date) -ge $receiptDeadline)
        if (-not $status) { throw "Update receipt not recovered within $TimeoutSeconds seconds. Do not reschedule blindly. Last connection error: $lastReceiptError" }
        if ($status.state -ne 'succeeded') {
            throw "Remote update failed: $($status.error)"
        }
        $resultLabel = if ($status.outcome -eq 'already_current') { 'Already current' } else { 'Updated' }
        $results.Add([pscustomobject]@{
            Device = $target.Name
            Hostname = $status.hostname
            Result = $resultLabel
            TargetPath = $status.targetPath
            Sha256 = $status.newSha256
            Version = $status.newVersion
            ProcessId = $status.processId
        })
        $cleanupScript = @"
`$ErrorActionPreference = 'Stop'
try {
    `$scheduler = New-Object -ComObject 'Schedule.Service'
    `$scheduler.Connect()
    `$scheduler.GetFolder('\').DeleteTask('Pierdesk Update $token', 0)
} catch {}
`$root = [System.IO.Path]::GetFullPath((Join-Path `$env:LOCALAPPDATA 'ClipFerry\updates'))
if (Test-Path -LiteralPath `$root -PathType Container) {
    foreach (`$candidate in @(Get-ChildItem -LiteralPath `$root -Directory)) {
        `$directory = [System.IO.Path]::GetFullPath(`$candidate.FullName)
        if ((Split-Path -Parent `$directory) -ine `$root -or (Split-Path -Leaf `$directory) -notmatch '^[0-9a-f]{32}$') { continue }
        `$statusPath = Join-Path `$directory 'status.json'
        if (-not (Test-Path -LiteralPath `$statusPath -PathType Leaf)) { continue }
        try { `$savedStatus = [System.IO.File]::ReadAllText(`$statusPath) | ConvertFrom-Json } catch { continue }
        if (`$savedStatus.state -eq 'succeeded') {
            Remove-Item -LiteralPath `$directory -Recurse -Force
        }
    }
}
"@
        try {
            Invoke-RemotePowerShell -Target $target -Script $cleanupScript | Out-Null
        }
        catch {
            Write-Host "[$($target.Name)] Update succeeded, but temporary-file cleanup failed: $($_.Exception.Message)" -ForegroundColor Yellow
        }
        Write-Host "[$($target.Name)] $resultLabel." -ForegroundColor Green
    }
    catch {
        $results.Add([pscustomobject]@{
            Device = $target.Name
            Hostname = $target.ExpectedHostname
            Result = 'Failed'
            TargetPath = '-'
            Sha256 = $sourceSha256
        })
        Write-Host "[$($target.Name)] $($_.Exception.Message)" -ForegroundColor Red
    }
}

Write-Host ''
$results | Format-Table -AutoSize
if (@($results | Where-Object { $_.Result -eq 'Failed' }).Count -gt 0) {
    exit 1
}
