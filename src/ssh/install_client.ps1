# Inputs are immutable PowerShell literals supplied by Pierdesk. This runs as the current user;
# it never installs a service or changes a machine-wide setting.
$cfStep='准备 Pierdesk SSH 连接工具'
if(!(Test-Path -LiteralPath $cfBundlePath -PathType Leaf)) {throw 'Pierdesk OpenSSH 安装包不存在'}

function Assert-CFPlainDirectory([string]$Path) {
    if(!(Test-Path -LiteralPath $Path)) {New-Item -ItemType Directory -Path $Path -Force | Out-Null}
    $item=Get-Item -LiteralPath $Path
    if(!$item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {throw "OpenSSH 目录链接被拒绝：$Path"}
}
function Test-CFHash([string]$Path,[string]$Expected) {
    if(!(Test-Path -LiteralPath $Path -PathType Leaf)) {return $false}
    return ((Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash -ceq $Expected)
}
function Test-CFClient([string]$Path) {
    if(!(Test-Path -LiteralPath $Path)) {return $false}
    if((Get-Item -LiteralPath $Path).Attributes -band [IO.FileAttributes]::ReparsePoint) {throw 'Existing per-user OpenSSH target link rejected'}
    return ((Test-CFHash (Join-Path $Path 'ssh.exe') $cfSshHash) -and
            (Test-CFHash (Join-Path $Path 'ssh-keygen.exe') $cfKeygenHash) -and
            (Test-CFHash (Join-Path $Path 'libcrypto.dll') $cfCryptoHash))
}

Assert-CFPlainDirectory $cfClientRoot
$runtimeRoot=Join-Path $cfClientRoot 'runtime'
Assert-CFPlainDirectory $runtimeRoot
$target=Join-Path $runtimeRoot $cfBundleVersion
$actualHash=(Get-FileHash -LiteralPath $cfBundlePath -Algorithm SHA256).Hash.ToUpperInvariant()
if($actualHash -cne $cfBundleHash) {throw 'Pierdesk OpenSSH 安装包完整性检查失败'}
if(Test-CFClient $target) {$cfClientReady=$true;return}

$stage=Join-Path $runtimeRoot ('.staging-'+[Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $stage | Out-Null
try {
    Expand-Archive -LiteralPath $cfBundlePath -DestinationPath $stage
    $source=Join-Path $stage 'OpenSSH-Win64'
    foreach($name in @('ssh.exe','ssh-keygen.exe','libcrypto.dll','LICENSE.txt','NOTICE.txt')) {
        if(!(Test-Path -LiteralPath (Join-Path $source $name) -PathType Leaf)) {throw "官方 OpenSSH 安装包缺少文件：$name"}
    }
    if(!(Test-CFClient $source)) {throw '官方 OpenSSH 客户端文件哈希检查失败'}
    foreach($name in @('ssh.exe','ssh-keygen.exe','libcrypto.dll')) {
        $signature=Get-AuthenticodeSignature -LiteralPath (Join-Path $source $name)
        if($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'Microsoft') {throw "官方 OpenSSH 文件签名检查失败：$name"}
    }
    [IO.File]::WriteAllText((Join-Path $source '.clipferry-client.json'),(@{version=$cfBundleVersion;sha256=$cfBundleHash;source='PowerShell/Win32-OpenSSH'} | ConvertTo-Json -Compress),[Text.UTF8Encoding]::new($false))
    if(Test-Path -LiteralPath $target) {
        if((Get-Item -LiteralPath $target).Attributes -band [IO.FileAttributes]::ReparsePoint) {throw 'Existing per-user OpenSSH target link rejected'}
        $quarantine=Join-Path $runtimeRoot ('.replaced-'+$cfBundleVersion+'-'+[Guid]::NewGuid().ToString('N'))
        Move-Item -LiteralPath $target -Destination $quarantine
    }
    Move-Item -LiteralPath $source -Destination $target
    if(!(Test-CFClient $target)) {throw 'Pierdesk SSH 连接工具未能完成安装'}
    $cfClientReady=$true
} finally {
    if(Test-Path -LiteralPath $stage) {Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue}
}
