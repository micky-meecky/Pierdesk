[CmdletBinding()]
param(
    [switch]$InitKey,
    [switch]$ForceKey,
    [switch]$SkipR2Upload,
    [switch]$DryRun,
    [string]$ConfigPath = (Join-Path $PSScriptRoot 'update-release.local.psd1'),
    [string]$ExePath,
    [string]$Version,
    [string]$OutputDir
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if (-not $ExePath) {
    $distClipferry = Join-Path $root 'dist\clipferry.exe'
    $releaseBin = Join-Path $root 'target\x86_64-pc-windows-msvc\release\pierdesk.exe'
    $ExePath = if (Test-Path -LiteralPath $distClipferry) { $distClipferry } else { $releaseBin }
}
if (-not (Test-Path -LiteralPath $ExePath -PathType Leaf)) {
    throw "Pierdesk executable not found: $ExePath. Run scripts\package-clipferry.ps1 first."
}

function Invoke-PierdeskCli {
    param(
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][string[]]$Arguments
    )
    $psi = [Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = $Executable
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $escaped = $Arguments | ForEach-Object {
        if ($_ -match '[\s"]') { '"' + ($_ -replace '(\\*)"', '$1$1\"' -replace '(\\+)$', '$1$1') + '"' } else { $_ }
    }
    $psi.Arguments = $escaped -join ' '
    $proc = [Diagnostics.Process]::Start($psi)
    try {
        $stdout = $proc.StandardOutput.ReadToEnd()
        $stderr = $proc.StandardError.ReadToEnd()
        $proc.WaitForExit()
        if ($proc.ExitCode -ne 0) {
            throw "Pierdesk CLI failed (exit $($proc.ExitCode)): $stderr $stdout"
        }
        return @($stdout -split "`r?`n" | Where-Object { $_ -ne '' })
    } finally {
        $proc.Dispose()
    }
}

if ($InitKey) {
    $keyPath = Join-Path $env:LOCALAPPDATA 'ClipFerry\release-signing\update-signing.pk8'
    if (Test-Path -LiteralPath $ConfigPath -PathType Leaf) {
        $cfg = Import-PowerShellDataFile -Path $ConfigPath
        if ($cfg.SigningKeyPath) { $keyPath = [Environment]::ExpandEnvironmentVariables([string]$cfg.SigningKeyPath) }
    }
    $args = @('update-keygen', '--output', $keyPath)
    if ($ForceKey) { $args += '--force' }
    $output = Invoke-PierdeskCli -Executable $ExePath -Arguments $args
    $output | ForEach-Object { Write-Host $_ }
    Write-Host ''
    Write-Host "Next: copy scripts\update-release.example.psd1 to $ConfigPath, set UpdatePublicKey, R2PublicBaseUrl and PrimaryManifestUrl."
    return
}

if (-not (Test-Path -LiteralPath $ConfigPath -PathType Leaf)) {
    throw "Missing local release config: $ConfigPath. Copy scripts\update-release.example.psd1 and fill in your key & Cloudflare R2 settings."
}
$config = Import-PowerShellDataFile -Path $ConfigPath
$keyPath = [Environment]::ExpandEnvironmentVariables([string]$config.SigningKeyPath)
$publicKey = ([string]$config.UpdatePublicKey).Trim()
$r2Bucket = if ($config.ContainsKey('R2Bucket') -and $config.R2Bucket) { ([string]$config.R2Bucket).Trim() } else { '' }
$mirrorBaseUrl = if ($config.ContainsKey('MirrorBaseUrl') -and $config.MirrorBaseUrl) {
    ([string]$config.MirrorBaseUrl).Trim().TrimEnd('/')
} elseif ($config.ContainsKey('R2PublicBaseUrl') -and $config.R2PublicBaseUrl) {
    ([string]$config.R2PublicBaseUrl).Trim().TrimEnd('/')
} else { '' }
$gitHubRepo = if ($config.ContainsKey('GitHubRepo') -and $config.GitHubRepo) { ([string]$config.GitHubRepo).Trim() } else { 'micky-meecky/Pierdesk' }

if (-not (Test-Path -LiteralPath $keyPath -PathType Leaf)) {
    throw "Signing private key not found: $keyPath. Run with -InitKey first."
}
if (-not $publicKey) {
    throw "UpdatePublicKey is empty in $ConfigPath."
}

if (-not $Version) {
    $releaseJson = Join-Path $root 'dist\release.json'
    if (Test-Path -LiteralPath $releaseJson -PathType Leaf) {
        $meta = Get-Content -LiteralPath $releaseJson -Raw | ConvertFrom-Json
        $Version = [string]$meta.version
    } else {
        $verLine = (Invoke-PierdeskCli -Executable $ExePath -Arguments @('--version')) -join ''
        if ($verLine -match '^Pierdesk\s+([0-9]+\.[0-9]+\.[0-9]+)$') { $Version = $Matches[1] }
    }
}
if (-not $Version -or $Version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') {
    throw "Could not determine valid X.Y.Z version (got '$Version')."
}

if (-not $OutputDir) {
    $OutputDir = Join-Path $root "dist\publish-v$Version"
}
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null

$publishedExe = Join-Path $OutputDir 'pierdesk.exe'
Copy-Item -LiteralPath $ExePath -Destination $publishedExe -Force
$hash = (Get-FileHash -LiteralPath $publishedExe -Algorithm SHA256).Hash.ToUpperInvariant()

$downloadUrls = @()
if ($mirrorBaseUrl) {
    $downloadUrls += "$mirrorBaseUrl/releases/v$Version/pierdesk.exe"
}
$downloadUrls += "https://github.com/$gitHubRepo/releases/download/v$Version/pierdesk.exe"
$notesUrl = "https://github.com/$gitHubRepo/releases/tag/v$Version"
$manifestPath = Join-Path $OutputDir 'pierdesk-update.json'
$publishedAt = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')

$signArgs = @(
    'update-sign-manifest',
    '--key', $keyPath,
    '--exe', $publishedExe,
    '--version', $Version,
    '--published-at', $publishedAt,
    '--notes-url', $notesUrl,
    '--output', $manifestPath
)
foreach ($url in $downloadUrls) {
    $signArgs += @('--url', $url)
}
$signOutput = Invoke-PierdeskCli -Executable $ExePath -Arguments $signArgs
$signOutput | ForEach-Object { Write-Host $_ }

$verifyOutput = Invoke-PierdeskCli -Executable $ExePath -Arguments @('update-verify-manifest', '--manifest', $manifestPath, '--public-key', $publicKey)
$verifyOutput | ForEach-Object { Write-Host $_ }

$stableManifestCopy = Join-Path $OutputDir 'stable.json'
Copy-Item -LiteralPath $manifestPath -Destination $stableManifestCopy -Force
"$hash  pierdesk.exe" | Set-Content -LiteralPath (Join-Path $OutputDir 'SHA256SUMS.txt') -Encoding ASCII

if ($r2Bucket -and $mirrorBaseUrl -and -not $SkipR2Upload) {
    $wrangler = Get-Command wrangler -ErrorAction SilentlyContinue
    if (-not $wrangler) {
        throw 'wrangler CLI was not found on PATH. Install Wrangler (`npm i -g wrangler` + `wrangler login`) or pass -SkipR2Upload to only generate artifacts.'
    }
    $binaryKey = "$r2Bucket/releases/v$Version/pierdesk.exe"
    $latestBinaryKey = "$r2Bucket/releases/latest/pierdesk.exe"
    $manifestKey = "$r2Bucket/updates/stable.json"
    if ($DryRun) {
        Write-Host "[DryRun] wrangler r2 object put $binaryKey --file `"$publishedExe`" --content-type application/vnd.microsoft.portable-executable --cache-control `"public, max-age=31536000, immutable`" --remote"
        Write-Host "[DryRun] wrangler r2 object put $latestBinaryKey --file `"$publishedExe`" --content-type application/vnd.microsoft.portable-executable --cache-control `"public, max-age=300`" --remote"
        Write-Host "[DryRun] wrangler r2 object put $manifestKey --file `"$stableManifestCopy`" --content-type application/json --cache-control `"public, max-age=60, must-revalidate`" --remote"
    } else {
        & wrangler r2 object put $binaryKey --file $publishedExe --content-type 'application/vnd.microsoft.portable-executable' --cache-control 'public, max-age=31536000, immutable' --remote
        if ($LASTEXITCODE -ne 0) { throw "Failed uploading $binaryKey to Cloudflare R2." }
        & wrangler r2 object put $latestBinaryKey --file $publishedExe --content-type 'application/vnd.microsoft.portable-executable' --cache-control 'public, max-age=300' --remote
        if ($LASTEXITCODE -ne 0) { throw "Failed uploading $latestBinaryKey to Cloudflare R2." }
        & wrangler r2 object put $manifestKey --file $stableManifestCopy --content-type 'application/json' --cache-control 'public, max-age=60, must-revalidate' --remote
        if ($LASTEXITCODE -ne 0) { throw "Failed uploading $manifestKey to Cloudflare R2." }
    }
}

Write-Host ''
Write-Host "Release bundle ready in: $OutputDir"
Write-Host "Upload to GitHub Release tag v$Version (fallback source): pierdesk.exe, pierdesk-update.json, SHA256SUMS.txt"
