$trustedProgramFiles=[Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFiles)
$trustedProgramData=[Environment]::GetFolderPath([Environment+SpecialFolder]::CommonApplicationData)
$cfToolRoot=Join-Path $trustedProgramFiles 'ClipFerry\OpenSSH\10.0.0.0p2'
$tool=Join-Path $cfToolRoot 'clipferry-sshd.exe'
$client=Join-Path $cfToolRoot 'ssh.exe'
$service=Get-Service ClipFerrySshd -ErrorAction SilentlyContinue
$accounts=@()
$admins=@(Get-LocalGroupMember -SID 'S-1-5-32-544' -ErrorAction SilentlyContinue | ForEach-Object {$_.SID.Value})
$currentSid=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$hiddenNames=@('ASPNET','DefaultAccount','Guest','WDAGUtilityAccount','defaultuser0')
foreach($user in @(Get-LocalUser | Where-Object {$_.Enabled -and $_.SID.Value -eq $currentSid -and $hiddenNames -notcontains $_.Name -and $_.Name -notmatch '^CodexSandbox(Offline|Online)$'})) {
    $accounts+=@{name=$user.Name;sid=$user.SID.Value;admin=($admins -contains $user.SID.Value)}
}
$port=45834
$issue=''
if(Test-Path -LiteralPath $tool) {
    $savedPreference=$ErrorActionPreference
    try {
        $ErrorActionPreference='Continue'
        & $tool -V 2>&1 | Out-Null
        $probeExit=[int]$LASTEXITCODE
    } finally {$ErrorActionPreference=$savedPreference}
    if($probeExit -ne 0) {$issue="PIERDESK_OPENSSH_INVALID: bundled sshd failed with exit $probeExit"}
}
$keys=@()
$published=Join-Path $trustedProgramData 'ClipFerry\ssh\clipferry-public.json'
if(Test-Path -LiteralPath $published) {
    try {
        $metadata=Get-Content -LiteralPath $published -Raw | ConvertFrom-Json
        $port=[int]$metadata.port
        $keys=@($metadata.hostKeys)
    } catch {$issue='Pierdesk SSH metadata needs to be refreshed by configuring access.'}
}
$firewall='unknown'
$loopbackOnly=$false
$networks=@()
try {
    foreach($profile in @(Get-NetConnectionProfile -ErrorAction Stop)) {
        $addresses=@(Get-NetIPAddress -InterfaceIndex $profile.InterfaceIndex -AddressFamily IPv4 -ErrorAction SilentlyContinue | ForEach-Object {$_.IPAddress})
        $networks+=@{index=$profile.InterfaceIndex;name=$profile.InterfaceAlias;category=$profile.NetworkCategory.ToString();addresses=@($addresses)}
    }
} catch {}
try {
    $listeners=@(Get-NetTCPConnection -LocalPort 45834 -State Listen -ErrorAction Stop)
    $serviceProcess=(Get-CimInstance Win32_Service -Filter "Name='ClipFerrySshd'" -ErrorAction Stop).ProcessId
    $loopbackOnly=($listeners.Count -gt 0 -and @($listeners | Where-Object {$_.LocalAddress -ne '127.0.0.1' -or $_.OwningProcess -ne $serviceProcess}).Count -eq 0)
} catch {}
try {
    $rules=@(Get-NetFirewallRule -Name 'ClipFerry-SSH-Control-In' -PolicyStore ActiveStore -ErrorAction Stop)
    $rule=$rules | Where-Object {$_.Enabled -eq 'True' -and $_.Action -eq 'Allow' -and $_.Direction -eq 'Inbound' -and $_.Profile.ToString() -match 'Public' -and $_.Profile.ToString() -match 'Private'} | Select-Object -First 1
    $firewall='missing'
    if($rule) {
        $ports=@($rule | Get-NetFirewallPortFilter)
        $addresses=@($rule | Get-NetFirewallAddressFilter)
        $programs=@($rule | Get-NetFirewallApplicationFilter)
        if(@($ports | Where-Object {$_.Protocol -eq 'TCP' -and $_.LocalPort -eq '45833'}).Count -and @($addresses | Where-Object {$_.RemoteAddress -contains 'LocalSubnet'}).Count -and @($programs | Where-Object {$_.Program -ieq $cfGatewayProgram}).Count) {$firewall='configured'}
    }
} catch {$firewall='unknown'}
@{installed=(Test-Path -LiteralPath $tool);clientInstalled=(Test-Path -LiteralPath $client);firewall=$firewall;loopbackOnly=$loopbackOnly;networks=@($networks);running=($null -ne $service -and $service.Status -eq 'Running');port=$port;accounts=@($accounts);hostKeys=@($keys);issue=$issue} | ConvertTo-Json -Depth 7 -Compress
