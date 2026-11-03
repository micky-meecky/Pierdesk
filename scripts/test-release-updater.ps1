param([Parameter(Mandatory=$true)][string]$ExePath)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ExePath = [IO.Path]::GetFullPath($ExePath)
$bytes = [IO.File]::ReadAllBytes($ExePath)
$pe = [BitConverter]::ToInt32($bytes,0x3c)
if ([BitConverter]::ToUInt16($bytes,$pe+24+68) -ne 2) { throw 'EXE is not Windows GUI subsystem; console may flash.' }
$directory = Join-Path ([IO.Path]::GetTempPath()) ('pierdesk-update-smoke-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $directory | Out-Null
$incoming = Join-Path $directory 'incoming.exe'
Copy-Item -LiteralPath $ExePath -Destination $incoming
Copy-Item -LiteralPath "$PSScriptRoot\fixtures\update-helper\remote-apply.ps1" -Destination $directory
foreach ($scenario in @('success','simulate-failure')) {
    $status = Join-Path $directory "$scenario.json"
    $arguments = 'update-apply "{0}" {1} "{2}" {3} "C:\test path\pierdesk.exe" --force' -f $incoming,('A'*64),$status,$scenario
    $process = Start-Process -FilePath $incoming -ArgumentList $arguments -WindowStyle Hidden -PassThru
    $null = $process.Handle
    if (-not $process.WaitForExit(15000)) { $process.Kill(); throw 'Isolated update helper timed out.' }
    $expected = if ($scenario -eq 'success') { 0 } else { 1 }
    if ($process.ExitCode -ne $expected) { throw "Wrong helper exit status: $($process.ExitCode)" }
    $receipt = Get-Content -LiteralPath $status -Raw | ConvertFrom-Json
    if ($receipt.consoleWindow -ne 0 -or -not $receipt.force -or $receipt.target -ne 'C:\test path\pierdesk.exe') {
        throw "Console or argument regression: $status"
    }
}
Copy-Item -LiteralPath "$PSScriptRoot\clipferry-remote-apply.ps1" -Destination (Join-Path $directory 'remote-apply.ps1') -Force
$status = Join-Path $directory 'real-script-negative.json'
$arguments = 'update-apply "{0}" {1} "{2}" WRONG-HOST-NEGATIVE-CONTROL' -f $incoming,('A'*64),$status
$process = Start-Process -FilePath $incoming -ArgumentList $arguments -WindowStyle Hidden -PassThru
$null = $process.Handle
if (-not $process.WaitForExit(15000)) { $process.Kill(); throw 'Real-script negative control timed out.' }
$receipt = Get-Content -LiteralPath $status -Raw | ConvertFrom-Json
if ($process.ExitCode -ne 1 -or $receipt.error -notlike 'Host identity mismatch:*' -or $receipt.targetPath) {
    throw 'Real updater must reject wrong host before locating or stopping the installed application.'
}
Write-Host "PASS GUI subsystem, no-console child, quoted path, force, success/failure exit codes. Evidence: $directory"
