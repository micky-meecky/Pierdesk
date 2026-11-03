[CmdletBinding()]
param(
    [ValidateSet('Patch','Minor','Major')][string]$Increment = 'Patch',
    [switch]$FormalRelease,
    [switch]$SkipRemoteDeploy,
    [string]$Cargo = (Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'),
    [string]$VcVars = 'Y:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat'
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\release-policy.ps1"
$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$manifest = Join-Path $root 'Cargo.toml'
$text = [IO.File]::ReadAllText($manifest)
$pattern = '(?m)^version = "([0-9]+\.[0-9]+\.[0-9]+)"'
if ([regex]::Matches($text, $pattern).Count -ne 1) { throw 'Ambiguous package version.' }
foreach ($tool in @($Cargo,$VcVars)) { if (-not (Test-Path -LiteralPath $tool)) { throw "Missing registered tool: $tool" } }
$target = Join-Path $root 'target'
New-Item -ItemType Directory -Force -Path $target | Out-Null
$lock = [IO.File]::Open((Join-Path $target 'package.lock'), 'OpenOrCreate', 'ReadWrite', 'None')
try {
    # Reserve before compiling; a failed packaging attempt does not reuse its version.
    $text = [IO.File]::ReadAllText($manifest)
    $current = [regex]::Match($text, $pattern).Groups[1].Value
    $next = Get-NextPierdeskVersion $current $Increment -FormalRelease:$FormalRelease
    [IO.File]::WriteAllText($manifest, [regex]::Replace($text, $pattern, "version = `"$next`""), [Text.UTF8Encoding]::new($false))
    Write-Host "Packaging Pierdesk $current -> $next"
    Push-Location $root
    try {
        & $env:ComSpec /d /c "`"$VcVars`" >nul && `"$Cargo`" build --offline --release --bin pierdesk"
        if ($LASTEXITCODE -ne 0) { throw "Release build failed; $next remains reserved. Previous dist preserved." }
        $binary = Join-Path $target 'x86_64-pc-windows-msvc\release\pierdesk.exe'
        $versionOutput = @(& $binary --version | ForEach-Object { "$_" }) -join ''
        if ($LASTEXITCODE -ne 0 -or $versionOutput -ne "Pierdesk $next") { throw "CLI version mismatch: $versionOutput" }
        if ((Get-Item -LiteralPath $binary).VersionInfo.FileVersion -ne "$next.0") { throw 'EXE resource version mismatch.' }
        & $binary tray-self-test
        if ($LASTEXITCODE -ne 0) { throw 'Release tray self-test failed.' }
        # Real ssh-keygen with this exact binary as its askpass helper; unit tests cannot cover it.
        $keygenSelfTest = @(& $binary ssh-keygen-self-test 2>&1 | ForEach-Object { "$_" }) -join ' '
        if ($LASTEXITCODE -ne 0 -or $keygenSelfTest -notmatch 'ssh-keygen self-test passed') { throw "Release ssh-keygen self-test failed: $keygenSelfTest" }
        & "$PSScriptRoot\test-release-updater.ps1" -ExePath $binary
        $hash = (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash
        $dist = Join-Path $root 'dist'
        New-Item -ItemType Directory -Force -Path $dist | Out-Null
        $incoming = Join-Path $dist 'pierdesk.package.tmp'
        Copy-Item -LiteralPath $binary -Destination $incoming -Force
        if ((Get-FileHash -LiteralPath $incoming).Hash -ne $hash) { throw 'Packaging copy checksum mismatch.' }
        Move-Item -LiteralPath $incoming -Destination (Join-Path $dist 'pierdesk.exe') -Force
        [ordered]@{version=$next; sha256=$hash; builtAt=(Get-Date).ToString('o'); file='pierdesk.exe'} |
            ConvertTo-Json | Set-Content -LiteralPath (Join-Path $dist 'release.json') -Encoding UTF8
        if ($SkipRemoteDeploy) {
            Write-Host 'Remote deployment skipped explicitly; artifact is for local testing.'
        } else {
            & "$PSScriptRoot\deploy-clipferry-ssh.ps1"
            if ($LASTEXITCODE -ne 0) { throw 'Artifact is built, but one or more remote deployments failed; do not report all machines updated.' }
        }
    } finally { Pop-Location }
} finally { $lock.Dispose() }
