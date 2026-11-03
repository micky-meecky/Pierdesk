$cfStep='配置 Pierdesk SSH 服务'
$trustedProgramData=[Environment]::GetFolderPath([Environment+SpecialFolder]::CommonApplicationData)
if([string]::IsNullOrWhiteSpace($trustedProgramData)) {throw 'Windows ProgramData path is unavailable'}
$trustedSystemDirectory=[Environment]::SystemDirectory
if([string]::IsNullOrWhiteSpace($trustedSystemDirectory) -or ![IO.Path]::IsPathRooted($trustedSystemDirectory)) {throw 'Windows system directory is unavailable'}
$sc=Join-Path $trustedSystemDirectory 'sc.exe'
if(!(Test-Path -LiteralPath $sc -PathType Leaf)) {throw 'Windows Service Control tool is unavailable'}
$cfDataRoot=Join-Path $trustedProgramData 'ClipFerry\ssh'
$cfConfig=Join-Path $cfDataRoot 'sshd_config'
$cfLogRoot=Join-Path $cfDataRoot 'logs'
$cfLog=Join-Path $cfLogRoot 'sshd.log'
$cfServiceName='ClipFerrySshd'
$cfPort=45834
$tool=Join-Path $cfToolRoot 'clipferry-sshd.exe'
$keygen=Join-Path $cfToolRoot 'ssh-keygen.exe'

function Protect-CFDirectory([string]$Path,[bool]$AllowUsersRead) {
    if(!(Test-Path -LiteralPath $Path)) {New-Item -ItemType Directory -Path $Path -Force | Out-Null}
    if((Get-Item -LiteralPath $Path).Attributes -band [IO.FileAttributes]::ReparsePoint) {throw "SSH directory link rejected: $Path"}
    $acl=[Security.AccessControl.DirectorySecurity]::new()
    $acl.SetAccessRuleProtection($true,$false)
    $acl.SetOwner([Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'))
    $inherit=[Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
    foreach($sid in @('S-1-5-18','S-1-5-32-544')) {
        [void]$acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new($sid),[Security.AccessControl.FileSystemRights]::FullControl,$inherit,[Security.AccessControl.PropagationFlags]::None,[Security.AccessControl.AccessControlType]::Allow))
    }
    if($AllowUsersRead) {
        [void]$acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new('S-1-5-32-545'),[Security.AccessControl.FileSystemRights]'ReadAndExecute, Synchronize',$inherit,[Security.AccessControl.PropagationFlags]::None,[Security.AccessControl.AccessControlType]::Allow))
    }
    Set-Acl -LiteralPath $Path -AclObject $acl
}
function Protect-CFPrivateFile([string]$Path) {
    $acl=[Security.AccessControl.FileSecurity]::new()
    $acl.SetAccessRuleProtection($true,$false)
    $acl.SetOwner([Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'))
    foreach($sid in @('S-1-5-18','S-1-5-32-544')) {
        [void]$acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new($sid),[Security.AccessControl.FileSystemRights]::FullControl,[Security.AccessControl.AccessControlType]::Allow))
    }
    Set-Acl -LiteralPath $Path -AclObject $acl
}
function Protect-CFPublicFile([string]$Path) {
    $acl=[Security.AccessControl.FileSecurity]::new()
    $acl.SetAccessRuleProtection($true,$false)
    $acl.SetOwner([Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'))
    foreach($sid in @('S-1-5-18','S-1-5-32-544')) {
        [void]$acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new($sid),[Security.AccessControl.FileSystemRights]::FullControl,[Security.AccessControl.AccessControlType]::Allow))
    }
    [void]$acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new('S-1-5-32-545'),[Security.AccessControl.FileSystemRights]::Read,[Security.AccessControl.AccessControlType]::Allow))
    Set-Acl -LiteralPath $Path -AclObject $acl
}
Protect-CFDirectory (Split-Path $cfDataRoot) $true
Protect-CFDirectory $cfDataRoot $true
Protect-CFDirectory (Join-Path $cfDataRoot 'accounts') $false
Protect-CFDirectory $cfLogRoot $false

$ed25519HostKey=Join-Path $cfDataRoot 'ssh_host_ed25519_key'
$rsaHostKey=Join-Path $cfDataRoot 'ssh_host_rsa_key'
$hostKey=if(Test-Path -LiteralPath $ed25519HostKey -PathType Leaf){$ed25519HostKey}elseif(Test-Path -LiteralPath $rsaHostKey -PathType Leaf){$rsaHostKey}else{$null}
if($null -eq $hostKey) {
    # Generate in a one-use directory owned by the exact elevated identity. Some Windows setups
    # refuse private-key creation directly below a directory owned by the Administrators group,
    # even though the elevated token can otherwise write there.
    $keyStage=Join-Path $cfDataRoot ('.host-key-'+[Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $keyStage | Out-Null
    try {
        $stageAcl=[Security.AccessControl.DirectorySecurity]::new()
        $stageAcl.SetAccessRuleProtection($true,$false)
        $elevatedSid=[Security.Principal.WindowsIdentity]::GetCurrent().User
        $stageAcl.SetOwner($elevatedSid)
        $stageInherit=[Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
        foreach($sid in @($elevatedSid,[Security.Principal.SecurityIdentifier]::new('S-1-5-18'),[Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'))) {
            [void]$stageAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($sid,[Security.AccessControl.FileSystemRights]::FullControl,$stageInherit,[Security.AccessControl.PropagationFlags]::None,[Security.AccessControl.AccessControlType]::Allow))
        }
        Set-Acl -LiteralPath $keyStage -AclObject $stageAcl

        $failures=@()
        foreach($attempt in @(
            @{name='Ed25519';type='ed25519';bits=$null;target=$ed25519HostKey},
            @{name='RSA-3072';type='rsa';bits='3072';target=$rsaHostKey}
        )) {
            $candidate=Join-Path $keyStage 'host_key'
            $arguments=@('-q','-t',$attempt.type)
            if($null -ne $attempt.bits) {$arguments+=@('-b',$attempt.bits)}
            # Windows PowerShell 5.1 drops a normal empty string when constructing a native
            # command line. A literal pair of quotes is the documented-compatible empty argv.
            $arguments+=@('-N','""','-f',$candidate)
            $savedPreference=$ErrorActionPreference
            try {
                $ErrorActionPreference='Continue'
                $nativeOutput=@(& $keygen @arguments 2>&1)
                $keygenExit=[int]$LASTEXITCODE
            } finally {$ErrorActionPreference=$savedPreference}
            if($keygenExit -eq 0 -and (Test-Path -LiteralPath $candidate -PathType Leaf)) {
                Move-Item -LiteralPath $candidate -Destination $attempt.target
                if(Test-Path -LiteralPath ($candidate+'.pub') -PathType Leaf) {Move-Item -LiteralPath ($candidate+'.pub') -Destination ($attempt.target+'.pub')}
                $hostKey=$attempt.target
                break
            }
            $details=($nativeOutput | ForEach-Object {$_.ToString().Trim()} | Where-Object {$_}) -join '；'
            if(!$details) {$details="退出代码 $keygenExit"}
            $failures+=($attempt.name+'：'+$details)
            Remove-Item -LiteralPath $candidate,($candidate+'.pub') -Force -ErrorAction SilentlyContinue
        }
        if($null -eq $hostKey) {throw ('无法生成 Pierdesk SSH 主机密钥。' + ($failures -join '；'))}
    } finally {
        if(Test-Path -LiteralPath $keyStage) {Remove-Item -LiteralPath $keyStage -Recurse -Force -ErrorAction SilentlyContinue}
    }
}
Protect-CFPrivateFile $hostKey
if(Test-Path -LiteralPath ($hostKey+'.pub')) {Protect-CFPublicFile ($hostKey+'.pub')}

$sftp=(Join-Path $cfToolRoot 'sftp-server.exe').Replace('\','/')
$hostKeyConfig=$hostKey.Replace('\','/')
$accountKeys=(Join-Path $cfDataRoot 'accounts/%u/authorized_keys').Replace('\','/')
$pidFile=(Join-Path $cfDataRoot 'sshd.pid').Replace('\','/')
$configText=@"
# Managed by ClipFerry. This file is independent from Windows OpenSSH. Schema 1.
Port $cfPort
AddressFamily inet
ListenAddress 127.0.0.1
PidFile $pidFile
HostKey $hostKeyConfig
PubkeyAuthentication yes
PasswordAuthentication no
PermitEmptyPasswords no
KbdInteractiveAuthentication no
DisableForwarding yes
AuthorizedKeysFile "$accountKeys"
Subsystem sftp "$sftp"
"@
$hadConfig=Test-Path -LiteralPath $cfConfig
if($hadConfig) {
    if((Get-Item -LiteralPath $cfConfig).Attributes -band [IO.FileAttributes]::ReparsePoint) {throw 'SSH config link rejected'}
    $existingConfig=[IO.File]::ReadAllText($cfConfig)
    # The marker is persisted in installed configs; it stays "ClipFerry" even after the product rename.
    if(!$existingConfig.StartsWith('# Managed by ClipFerry.') -and !$existingConfig.StartsWith('# Managed by Pierdesk.')) {throw 'Pierdesk SSH 配置文件已被其他程序替换，请移走后重试'}
}
$configNext=$cfConfig+'.next-'+[Guid]::NewGuid().ToString('N')
$configBackup=$cfConfig+'.backup-'+[Guid]::NewGuid().ToString('N')
$rollbackDiscard=$cfConfig+'.failed-'+[Guid]::NewGuid().ToString('N')
$configInstalled=$false
$configCommitted=$false
try {
    [IO.File]::WriteAllText($configNext,$configText,[Text.UTF8Encoding]::new($false))
    Protect-CFPrivateFile $configNext

    $savedPreference=$ErrorActionPreference
    try {
        $ErrorActionPreference='Continue'
        $validation=@(& $tool -t -f $configNext 2>&1)
        $validationExit=[int]$LASTEXITCODE
    } finally {$ErrorActionPreference=$savedPreference}
    if($validationExit) {
        $details=($validation | ForEach-Object {$_.ToString().Trim()} | Where-Object {$_}) -join '；'
        if(!$details) {$details="sshd exited with code $validationExit"}
        throw "Pierdesk SSH 配置检查失败：$details"
    }
    if($hadConfig) {[IO.File]::Replace($configNext,$cfConfig,$configBackup)}
    else {[IO.File]::Move($configNext,$cfConfig)}
    $configInstalled=$true
    Protect-CFPrivateFile $cfConfig

$binaryPath='"'+$tool+'" -f "'+$cfConfig+'" -E "'+$cfLog+'"'
$existing=Get-CimInstance Win32_Service -Filter "Name='$cfServiceName'" -ErrorAction SilentlyContinue
if($null -eq $existing) {
    New-Service -Name $cfServiceName -DisplayName 'Pierdesk SSH Server' -Description 'SSH access explicitly authorized through Pierdesk.' -BinaryPathName $binaryPath -StartupType Automatic | Out-Null
} else {
    if($existing.PathName -ne $binaryPath) {
        Stop-Service -Name $cfServiceName -Force -ErrorAction SilentlyContinue
        & $sc config $cfServiceName 'binPath=' $binaryPath | Out-Null
        if($LASTEXITCODE) {throw '无法更新 Pierdesk SSH 服务路径'}
    }
    Set-Service -Name $cfServiceName -StartupType Automatic
}
& $sc config $cfServiceName 'obj=' 'LocalSystem' | Out-Null
if($LASTEXITCODE) {throw '无法把 Pierdesk SSH 服务设置为 LocalSystem'}
& $sc privs $cfServiceName 'SeAssignPrimaryTokenPrivilege/SeTcbPrivilege/SeBackupPrivilege/SeRestorePrivilege/SeImpersonatePrivilege' | Out-Null
if($LASTEXITCODE) {throw '无法配置 Pierdesk SSH 服务权限'}
& $sc failure $cfServiceName 'reset=' '86400' 'actions=' 'restart/5000/restart/15000/none/0' | Out-Null

# Match the RedirectionGuard mitigation created by the upstream Microsoft installer, scoped to
# Pierdesk's app-specific service image name so no system or third-party sshd.exe is affected.
$mitigationPath='HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\clipferry-sshd.exe'
if(!(Test-Path -LiteralPath $mitigationPath)) {New-Item -Path $mitigationPath -Force | Out-Null}
$mitigation=Get-ItemProperty -LiteralPath $mitigationPath -Name MitigationOptions -ErrorAction SilentlyContinue
if($null -eq $mitigation) {
    $redirectionGuard=[byte[]](0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0x10)
    New-ItemProperty -LiteralPath $mitigationPath -Name MitigationOptions -PropertyType Binary -Value $redirectionGuard | Out-Null
}

# Only the certificate-authenticated application gateway is reachable from the LAN.
# LocalSubnet limits exposure, not trust. SSH itself is loopback-only.
if([string]::IsNullOrWhiteSpace($cfGatewayProgram) -or !(Test-Path -LiteralPath $cfGatewayProgram -PathType Leaf)) {throw 'Pierdesk gateway executable is unavailable'}
$gatewayRule=Get-NetFirewallRule -Name 'ClipFerry-SSH-Control-In' -ErrorAction SilentlyContinue
if(!$gatewayRule) {
    New-NetFirewallRule -Name 'ClipFerry-SSH-Control-In' -DisplayName 'Pierdesk authenticated SSH gateway' -Direction Inbound -Action Allow -Protocol TCP -LocalPort 45833 -Profile Public,Private -RemoteAddress LocalSubnet -Program $cfGatewayProgram -EdgeTraversalPolicy Block | Out-Null
} else {
    $gatewayRule | Set-NetFirewallRule -Enabled True -Direction Inbound -Action Allow -Profile Public,Private -Program $cfGatewayProgram -EdgeTraversalPolicy Block | Out-Null
    $gatewayRule | Get-NetFirewallPortFilter | Set-NetFirewallPortFilter -Protocol TCP -LocalPort 45833 | Out-Null
    $gatewayRule | Get-NetFirewallAddressFilter | Set-NetFirewallAddressFilter -LocalAddress Any -RemoteAddress LocalSubnet | Out-Null
}
Get-NetFirewallRule -Name 'ClipFerry-SSH-In' -ErrorAction SilentlyContinue | Remove-NetFirewallRule

$cfStep='启动 Pierdesk SSH 服务'
$serviceRecord=Get-CimInstance Win32_Service -Filter "Name='$cfServiceName'"
if($serviceRecord.StartName -ne 'LocalSystem') {throw 'Pierdesk SSH 服务账户不是 LocalSystem'}
$current=Get-Service -Name $cfServiceName
if($current.Status -eq 'Running') {Restart-Service -Name $cfServiceName -Force}
else {Start-Service -Name $cfServiceName}
$deadline=[DateTime]::UtcNow.AddSeconds(15)
do {
    Start-Sleep -Milliseconds 200
    $current=Get-Service -Name $cfServiceName
} while($current.Status -ne 'Running' -and [DateTime]::UtcNow -lt $deadline)
if($current.Status -ne 'Running') {
    $details='Windows 没有返回服务日志'
    if(Test-Path -LiteralPath $cfLog) {
        $details=((Get-Content -LiteralPath $cfLog -Tail 12 -ErrorAction SilentlyContinue) -join '；')
        if($details.Length -gt 1500) {$details=$details.Substring($details.Length-1500)}
    }
    throw "Pierdesk SSH 服务未能启动：$details"
}
$listeners=@(Get-NetTCPConnection -LocalPort $cfPort -State Listen -ErrorAction Stop)
$serviceProcess=(Get-CimInstance Win32_Service -Filter "Name='$cfServiceName'").ProcessId
if(!$listeners.Count -or @($listeners | Where-Object {$_.LocalAddress -ne '127.0.0.1' -or $_.OwningProcess -ne $serviceProcess}).Count) {throw 'SSH loopback-only listener verification failed'}
$effectiveRule=Get-NetFirewallRule -Name 'ClipFerry-SSH-Control-In' -PolicyStore ActiveStore -ErrorAction Stop
if($effectiveRule.Enabled -ne 'True' -or $effectiveRule.Action -ne 'Allow' -or $effectiveRule.Profile.ToString() -notmatch 'Public' -or $effectiveRule.Profile.ToString() -notmatch 'Private') {throw 'Authenticated gateway firewall policy verification failed'}
$configCommitted=$true
} catch {
    $setupFailure=$_
    if($configInstalled) {
        try {
            if($hadConfig -and (Test-Path -LiteralPath $configBackup)) {
                if(Test-Path -LiteralPath $cfConfig) {[IO.File]::Replace($configBackup,$cfConfig,$rollbackDiscard)}
                else {[IO.File]::Move($configBackup,$cfConfig)}
                Protect-CFPrivateFile $cfConfig
                $rollbackService=Get-Service -Name $cfServiceName -ErrorAction SilentlyContinue
                if($rollbackService) {
                    if($rollbackService.Status -eq 'Running') {Restart-Service -Name $cfServiceName -Force -ErrorAction SilentlyContinue}
                    else {Start-Service -Name $cfServiceName -ErrorAction SilentlyContinue}
                }
            } elseif(!$hadConfig -and (Test-Path -LiteralPath $cfConfig)) {
                Remove-Item -LiteralPath $cfConfig -Force
            }
        } catch {}
    }
    throw $setupFailure
} finally {
    if(Test-Path -LiteralPath $configNext) {Remove-Item -LiteralPath $configNext -Force -ErrorAction SilentlyContinue}
    if($configCommitted -and (Test-Path -LiteralPath $configBackup)) {Remove-Item -LiteralPath $configBackup -Force -ErrorAction SilentlyContinue}
    if(Test-Path -LiteralPath $rollbackDiscard) {Remove-Item -LiteralPath $rollbackDiscard -Force -ErrorAction SilentlyContinue}
}
