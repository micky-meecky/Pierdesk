Set-StrictMode -Version Latest

function Get-NextPierdeskVersion {
    param([string]$Current, [ValidateSet('Patch','Minor','Major')][string]$Increment = 'Patch', [switch]$FormalRelease)
    if ($Current -notmatch '^([0-9]+)\.([0-9]+)\.([0-9]+)$') { throw 'Expected a three-component numeric version.' }
    $major = [int]$Matches[1]; $minor = [int]$Matches[2]; $patch = [int]$Matches[3]
    switch ($Increment) {
        'Patch' { $patch++ }
        'Minor' { $minor++; $patch = 0 }
        'Major' {
            if (-not $FormalRelease) { throw 'Major increments require explicit -FormalRelease authorization.' }
            $major++; $minor = 0; $patch = 0
        }
    }
    if (@($major,$minor,$patch | Where-Object { $_ -gt 65535 }).Count -gt 0) { throw 'Windows version component overflow.' }
    return "$major.$minor.$patch"
}
