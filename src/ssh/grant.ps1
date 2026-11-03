# Inputs are immutable PowerShell literals supplied by the local UI operation.
$cfStep=if($revoke){'撤销 SSH 授权'}else{'写入 SSH 授权'}
function Write-CFAtomic($path,$text) {
    $next=$path+'.pierdesk-'+[Guid]::NewGuid().ToString('N')
    $backup=$path+'.pierdesk-backup-'+[Guid]::NewGuid().ToString('N')
    try {
        [IO.File]::WriteAllText($next,$text,[Text.UTF8Encoding]::new($false))
        if(Test-Path -LiteralPath $path) {
            [IO.File]::Replace($next,$path,$backup)
            [IO.File]::Delete($backup)
        } else {[IO.File]::Move($next,$path)}
    } finally {
        if(Test-Path -LiteralPath $next){[IO.File]::Delete($next)}
        if(Test-Path -LiteralPath $backup){[IO.File]::Delete($backup)}
    }
}
function Get-CFUpdatedKeys($text,$entry,$remove) {
    $lines=@($text -split '\r?\n' | Where-Object {$_ -and $_ -cne $entry})
    if(!$remove){$lines+=$entry}
    if(!$lines.Count){return ''}
    return ($lines -join "`r`n")+"`r`n"
}
function Protect-CFPath($path,$directory,$readerSid) {
    $acl=if($directory){[Security.AccessControl.DirectorySecurity]::new()}else{[Security.AccessControl.FileSecurity]::new()}
    $acl.SetAccessRuleProtection($true,$false)
    $acl.SetOwner([Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'))
    foreach($sid in @('S-1-5-18','S-1-5-32-544')) {
        $identity=[Security.Principal.SecurityIdentifier]::new($sid)
        $rule=if($directory){[Security.AccessControl.FileSystemAccessRule]::new($identity,'FullControl','ContainerInherit,ObjectInherit','None','Allow')}else{[Security.AccessControl.FileSystemAccessRule]::new($identity,'FullControl','Allow')}
        $acl.AddAccessRule($rule)
    }
    if($readerSid) {
        $identity=[Security.Principal.SecurityIdentifier]::new($readerSid)
        $rule=if($directory){[Security.AccessControl.FileSystemAccessRule]::new($identity,'ReadAndExecute','ContainerInherit,ObjectInherit','None','Allow')}else{[Security.AccessControl.FileSystemAccessRule]::new($identity,'Read','Allow')}
        $acl.AddAccessRule($rule)
    }
    Set-Acl -LiteralPath $path -AclObject $acl
}
function Assert-CFPlainPath($path,$label) {
    if(!(Test-Path -LiteralPath $path)){throw "$label is missing; set up local SSH access first"}
    if((Get-Item -LiteralPath $path).Attributes -band [IO.FileAttributes]::ReparsePoint){throw "$label link rejected"}
}

$mutex=[Threading.Mutex]::new($false,'Global\ClipFerrySshAuthorizationV1')
if(!$mutex.WaitOne(30000)){throw 'Another SSH configuration operation is running'}
try {
    $user=Get-LocalUser -SID $accountSid
    if(!$user.Enabled){throw 'Account disabled'}
    $name=$user.Name.ToLowerInvariant()
    if($name -notmatch '^[\p{L}\p{Nd}_.-]+$'){throw 'This account name requires manual SSH configuration'}

    $cfServiceName='ClipFerrySshd'
    $trustedProgramFiles=[Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFiles)
    $trustedProgramData=[Environment]::GetFolderPath([Environment+SpecialFolder]::CommonApplicationData)
    if([string]::IsNullOrWhiteSpace($trustedProgramFiles) -or [string]::IsNullOrWhiteSpace($trustedProgramData)){throw 'Trusted Windows data paths are unavailable'}
    $toolRoot=Join-Path $trustedProgramFiles 'ClipFerry\OpenSSH\10.0.0.0p2'
    $tool=Join-Path $toolRoot 'sshd.exe'
    $dataRoot=Join-Path $trustedProgramData 'ClipFerry\ssh'
    $config=Join-Path $dataRoot 'sshd_config'
    $accountsRoot=Join-Path $dataRoot 'accounts'
    $accountDir=Join-Path $accountsRoot $name
    $keyPath=Join-Path $accountDir 'authorized_keys'

    Assert-CFPlainPath $toolRoot 'Pierdesk OpenSSH directory'
    Assert-CFPlainPath $tool 'Pierdesk sshd'
    Assert-CFPlainPath $dataRoot 'Pierdesk SSH data directory'
    Assert-CFPlainPath $config 'Pierdesk SSH configuration'
    if(!(Get-Service -Name $cfServiceName -ErrorAction SilentlyContinue)){throw 'Pierdesk SSH service is missing; set up local SSH access first'}

    if(!(Test-Path -LiteralPath $accountsRoot)){New-Item -ItemType Directory -Path $accountsRoot | Out-Null}
    Assert-CFPlainPath $accountsRoot 'Pierdesk SSH accounts directory'
    Protect-CFPath $accountsRoot $true $null
    if(!(Test-Path -LiteralPath $accountDir)){New-Item -ItemType Directory -Path $accountDir | Out-Null}
    Assert-CFPlainPath $accountDir 'SSH account directory'
    Protect-CFPath $accountDir $true $accountSid
    if(Test-Path -LiteralPath $keyPath){Assert-CFPlainPath $keyPath 'SSH authorization file'}

    $hadKey=Test-Path -LiteralPath $keyPath
    $oldKeys=if($hadKey){[IO.File]::ReadAllText($keyPath)}else{''}
    $entry=$publicKey+' clipferry:'+$grantId
    $newKeys=Get-CFUpdatedKeys $oldKeys $entry $revoke
    try {
        Write-CFAtomic $keyPath $newKeys
        Assert-CFPlainPath $keyPath 'SSH authorization file'
        Protect-CFPath $keyPath $false $accountSid

        $installed=@([IO.File]::ReadAllText($keyPath) -split '\r?\n' | Where-Object {$_ -ceq $entry})
        if((!$revoke -and $installed.Count -ne 1) -or ($revoke -and $installed.Count -ne 0)){throw 'SSH authorization update verification failed'}

        $savedPreference=$ErrorActionPreference
        $ErrorActionPreference='Continue'
        try {
            $effective=@(& $tool -T -f $config -C ('user='+$name+',host=localhost,addr=127.0.0.1') 2>&1)
            $probeExit=$LASTEXITCODE
        } finally {$ErrorActionPreference=$savedPreference}
        if($probeExit){throw 'Cannot validate Pierdesk SSH account rules'}
        $rules=@($effective | Where-Object {$_ -match '^authorizedkeysfile '})
        if($rules.Count -ne 1){throw 'Pierdesk SSH account rule is unavailable'}
        $first=(($rules[0].Substring(19).Trim() -split '\s+')[0]).Trim('"').Replace('%u',$name).Replace('\','/')
        $expected=$keyPath.Replace('\','/')
        if($first -ine $expected){throw 'Pierdesk SSH account isolation validation failed'}
    } catch {
        if($hadKey){Write-CFAtomic $keyPath $oldKeys;Protect-CFPath $keyPath $false $accountSid}else{[IO.File]::Delete($keyPath)}
        throw
    }
} finally {$mutex.ReleaseMutex();$mutex.Dispose()}
