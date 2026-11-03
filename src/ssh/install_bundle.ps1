# Inputs are immutable PowerShell literals supplied by Pierdesk. The archive is embedded in the
# signed application, and this elevated installer verifies its fixed release digest before use.
$cfStep='安装 Pierdesk OpenSSH 运行环境'
if(!(Test-Path -LiteralPath $cfBundlePath -PathType Leaf)) {throw 'Pierdesk OpenSSH 安装包不存在'}

$trustedProgramFiles=[Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFiles)
if([string]::IsNullOrWhiteSpace($trustedProgramFiles)) {throw 'Windows Program Files path is unavailable'}
$programRoot=Join-Path $trustedProgramFiles 'ClipFerry\OpenSSH'
$target=Join-Path $programRoot $cfBundleVersion
function Protect-CFOpenSshDirectory([string]$Path,[bool]$AllowUsersRead) {
    if(!(Test-Path -LiteralPath $Path)) {New-Item -ItemType Directory -Path $Path -Force | Out-Null}
    $item=Get-Item -LiteralPath $Path
    if($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {throw "OpenSSH directory link rejected: $Path"}
    $acl=[Security.AccessControl.DirectorySecurity]::new()
    $acl.SetAccessRuleProtection($true,$false)
    $acl.SetOwner([Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'))
    $inherit=[Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
    $entries=@(
        @{sid='S-1-5-18';rights=[Security.AccessControl.FileSystemRights]::FullControl},
        @{sid='S-1-5-32-544';rights=[Security.AccessControl.FileSystemRights]::FullControl}
    )
    if($AllowUsersRead) {$entries+=@{sid='S-1-5-32-545';rights=[Security.AccessControl.FileSystemRights]'ReadAndExecute, Synchronize'}}
    foreach($entry in $entries) {
        $identity=[Security.Principal.SecurityIdentifier]::new($entry.sid)
        $rule=[Security.AccessControl.FileSystemAccessRule]::new($identity,$entry.rights,$inherit,[Security.AccessControl.PropagationFlags]::None,[Security.AccessControl.AccessControlType]::Allow)
        [void]$acl.AddAccessRule($rule)
    }
    Set-Acl -LiteralPath $Path -AclObject $acl
}
function Protect-CFOpenSshFile([string]$Path) {
    $item=Get-Item -LiteralPath $Path
    if($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {throw "OpenSSH file link rejected: $Path"}
    $acl=[Security.AccessControl.FileSecurity]::new()
    $acl.SetAccessRuleProtection($true,$false)
    $acl.SetOwner([Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'))
    foreach($entry in @(
        @{sid='S-1-5-18';rights=[Security.AccessControl.FileSystemRights]::FullControl},
        @{sid='S-1-5-32-544';rights=[Security.AccessControl.FileSystemRights]::FullControl},
        @{sid='S-1-5-32-545';rights=[Security.AccessControl.FileSystemRights]'ReadAndExecute, Synchronize'}
    )) {
        [void]$acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new($entry.sid),$entry.rights,[Security.AccessControl.AccessControlType]::Allow))
    }
    Set-Acl -LiteralPath $Path -AclObject $acl
}
function Protect-CFOpenSshTree([string]$Path) {
    Protect-CFOpenSshDirectory $Path $true
    foreach($directory in @(Get-ChildItem -LiteralPath $Path -Directory -Recurse -Force)) {Protect-CFOpenSshDirectory $directory.FullName $true}
    foreach($file in @(Get-ChildItem -LiteralPath $Path -File -Recurse -Force)) {Protect-CFOpenSshFile $file.FullName}
}

Protect-CFOpenSshDirectory $programRoot $true
$privateStage=Join-Path $programRoot '.install'
Protect-CFOpenSshDirectory $privateStage $false
$protectedBundle=Join-Path $privateStage ('.bundle-'+[Guid]::NewGuid().ToString('N')+'.zip')
Copy-Item -LiteralPath $cfBundlePath -Destination $protectedBundle
try {
$actualHash=(Get-FileHash -LiteralPath $protectedBundle -Algorithm SHA256).Hash.ToUpperInvariant()
if($actualHash -ne $cfBundleHash) {throw 'Pierdesk OpenSSH 安装包完整性检查失败'}
$marker=Join-Path $target '.clipferry-bundle.json'
$ready=$false
if(Test-Path -LiteralPath $target) {
    if((Get-Item -LiteralPath $target).Attributes -band [IO.FileAttributes]::ReparsePoint) {throw 'Existing OpenSSH target link rejected'}
}
if(Test-Path -LiteralPath $marker -PathType Leaf) {
    try {
        $record=Get-Content -LiteralPath $marker -Raw | ConvertFrom-Json
        $ready=($record.sha256 -eq $cfBundleHash)
        foreach($name in @('ssh.exe','ssh-keygen.exe','sshd.exe','clipferry-sshd.exe','sshd-auth.exe','sshd-session.exe','libcrypto.dll','sftp-server.exe')) {
            if(!(Test-Path -LiteralPath (Join-Path $target $name) -PathType Leaf)) {$ready=$false}
        }
    } catch {$ready=$false}
}
if($ready) {
    foreach($name in @('ssh.exe','ssh-keygen.exe','sshd.exe','clipferry-sshd.exe','sshd-auth.exe','sshd-session.exe','libcrypto.dll','sftp-server.exe')) {
        $path=Join-Path $target $name
        if((Get-Item -LiteralPath $path).Attributes -band [IO.FileAttributes]::ReparsePoint) {$ready=$false;break}
        $signature=Get-AuthenticodeSignature -LiteralPath $path
        if($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'Microsoft') {$ready=$false;break}
    }
}
if(!$ready) {
    $stage=Join-Path $privateStage ('.staging-'+[Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $stage | Out-Null
    try {
        Expand-Archive -LiteralPath $protectedBundle -DestinationPath $stage
        $source=Join-Path $stage 'OpenSSH-Win64'
        foreach($name in @('ssh.exe','ssh-keygen.exe','sshd.exe','sshd-auth.exe','sshd-session.exe','libcrypto.dll','sftp-server.exe','sshd_config_default')) {
            $path=Join-Path $source $name
            if(!(Test-Path -LiteralPath $path -PathType Leaf)) {throw "官方 OpenSSH 安装包缺少文件：$name"}
        }
        foreach($name in @('ssh.exe','ssh-keygen.exe','sshd.exe','sshd-auth.exe','sshd-session.exe','libcrypto.dll')) {
            $signature=Get-AuthenticodeSignature -LiteralPath (Join-Path $source $name)
            if($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'Microsoft') {
                throw "官方 OpenSSH 文件签名检查失败：$name"
            }
        }
        # Give the service executable an app-specific image name. Windows process mitigation rules
        # are keyed by image name, so this keeps Pierdesk's required rule isolated from any system
        # or administrator-managed sshd.exe installation.
        Copy-Item -LiteralPath (Join-Path $source 'sshd.exe') -Destination (Join-Path $source 'clipferry-sshd.exe')
        $dedicatedSignature=Get-AuthenticodeSignature -LiteralPath (Join-Path $source 'clipferry-sshd.exe')
        if($dedicatedSignature.Status -ne 'Valid' -or $dedicatedSignature.SignerCertificate.Subject -notmatch 'Microsoft') {
            throw 'Pierdesk 专用 SSH 服务文件签名检查失败'
        }
        $savedPreference=$ErrorActionPreference
        try {
            $ErrorActionPreference='Continue'
            & (Join-Path $source 'ssh.exe') -V 2>&1 | Out-Null
            $clientExit=[int]$LASTEXITCODE
            & (Join-Path $source 'sshd.exe') -V 2>&1 | Out-Null
            $serverExit=[int]$LASTEXITCODE
        } finally {$ErrorActionPreference=$savedPreference}
        if($clientExit -ne 0 -or $serverExit -ne 0) {throw "官方 OpenSSH 程序自检失败（client=$clientExit，server=$serverExit）"}
        [IO.File]::WriteAllText((Join-Path $source '.clipferry-bundle.json'),(@{version=$cfBundleVersion;sha256=$cfBundleHash;source='PowerShell/Win32-OpenSSH'} | ConvertTo-Json -Compress),[Text.UTF8Encoding]::new($false))
        Protect-CFOpenSshDirectory $source $true
        if(Test-Path -LiteralPath $target) {
            if((Get-Item -LiteralPath $target).Attributes -band [IO.FileAttributes]::ReparsePoint) {throw 'Existing OpenSSH target link rejected'}
            Stop-Service -Name ClipFerrySshd -Force -ErrorAction SilentlyContinue
            $quarantine=Join-Path $programRoot ('.replaced-'+$cfBundleVersion+'-'+[Guid]::NewGuid().ToString('N'))
            Move-Item -LiteralPath $target -Destination $quarantine
        }
        Move-Item -LiteralPath $source -Destination $target
        Protect-CFOpenSshDirectory $target $true
        $ready=$true
    } finally {
        if(Test-Path -LiteralPath $stage) {Remove-Item -LiteralPath $stage -Recurse -Force}
    }
}
if(!$ready) {throw 'Pierdesk OpenSSH 运行环境未能完成安装'}
Protect-CFOpenSshTree $target
$cfToolRoot=$target
} finally {
    if(Test-Path -LiteralPath $protectedBundle) {Remove-Item -LiteralPath $protectedBundle -Force -ErrorAction SilentlyContinue}
}
