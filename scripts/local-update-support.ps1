# Local-only recovery, independently tested with disposable files. Never replaces
# an existing installation or touches credentials / previous-version backups.
function Initialize-LocalPierdeskTarget {
    param([string]$TargetPath, [string]$StagedPath, [string]$ExpectedHash)
    if ((Split-Path -Leaf $TargetPath) -ine 'pierdesk.exe') { throw 'Unexpected local install filename.' }
    if (Test-Path -LiteralPath $TargetPath -PathType Leaf) { return $false }
    $temporary = Join-Path (Split-Path -Parent $TargetPath) ('.pierdesk.restore.' + [guid]::NewGuid().ToString('N') + '.tmp')
    try {
        Copy-Item -LiteralPath $StagedPath -Destination $temporary
        if ((Get-FileHash -LiteralPath $temporary -Algorithm SHA256).Hash -ine $ExpectedHash) { throw 'Local install recovery checksum mismatch.' }
        # Move without overwrite: a concurrent newly-created target is preserved.
        [IO.File]::Move($temporary, $TargetPath)
        return $true
    } finally {
        if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force }
    }
}
