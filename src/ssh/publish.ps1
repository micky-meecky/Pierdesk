# Publish only public connection metadata so an unelevated UI can read the dedicated port and
# host identity. setup_portable.ps1 supplies the protected tool/config/data paths.
$cfStep='读取 Pierdesk SSH 公共连接信息'
$tool=Join-Path $cfToolRoot 'sshd.exe'
$keygen=Join-Path $cfToolRoot 'ssh-keygen.exe'
$effective=@(& $tool -T -f $cfConfig)
if($LASTEXITCODE) {throw 'Cannot inspect effective Pierdesk SSH configuration'}
$ports=@($effective | Where-Object {$_ -match '^port '})
if($ports.Count -ne 1) {throw 'Pierdesk SSH configuration must contain one port'}
$hostKeys=@()
foreach($line in @($effective | Where-Object {$_ -match '^hostkey '})) {
    $private=$line.Substring(8).Trim().Trim('"')
    $public=& $keygen -y -f $private
    if($LASTEXITCODE) {throw 'Cannot read Pierdesk SSH host public key'}
    $parts=@(([string]$public).Trim() -split '\s+')
    if($parts.Count -lt 2 -or $parts[0] -notin @('ssh-ed25519','ecdsa-sha2-nistp256','ssh-rsa') -or $parts[1] -notmatch '^[A-Za-z0-9+/]+={0,2}$') {throw 'Invalid Pierdesk SSH host public key'}
    $hostKeys+=($parts[0]+' '+$parts[1])
}
if(!$hostKeys.Count) {throw 'Pierdesk SSH host identity unavailable'}
$path=Join-Path $cfDataRoot 'clipferry-public.json'
if((Test-Path -LiteralPath $path) -and ((Get-Item -LiteralPath $path).Attributes -band [IO.FileAttributes]::ReparsePoint)) {throw 'Metadata link rejected'}
$json=@{port=[int]($ports[0] -split ' ')[1];hostKeys=@($hostKeys);runtime=$cfBundleVersion} | ConvertTo-Json -Compress
if(!(Test-Path -LiteralPath $path)){[IO.File]::WriteAllText($path,'{}')}
$acl=[Security.AccessControl.FileSecurity]::new()
$acl.SetAccessRuleProtection($true,$false)
$acl.SetOwner([Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'))
foreach($sid in @('S-1-5-18','S-1-5-32-544')) {[void]$acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new($sid),'FullControl','Allow'))}
[void]$acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new('S-1-5-32-545'),'Read','Allow'))
Set-Acl -LiteralPath $path -AclObject $acl
[IO.File]::WriteAllText($path,$json,[Text.UTF8Encoding]::new($false))
