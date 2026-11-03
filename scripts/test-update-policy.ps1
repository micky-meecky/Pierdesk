# Bounded, side-effect-free updater regression: load only function definitions, never
# execute the apply script or touch a running tray / network / installed EXE.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\release-policy.ps1"
$count = 0
function Assert($Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
    $script:count++
}
Assert ((Get-NextPierdeskVersion '1.0.0') -eq '1.0.1') 'Patch increment'
Assert ((Get-NextPierdeskVersion '1.8.9' 'Minor') -eq '1.9.0') 'Minor reset'
Assert ((Get-NextPierdeskVersion '1.8.9' 'Major' -FormalRelease) -eq '2.0.0') 'Formal major reset'
$rejected = $false; try { Get-NextPierdeskVersion '1.0.0' 'Major' | Out-Null } catch { $rejected = $true }
Assert $rejected 'Major without formal authorization must fail'
$rejected = $false; try { Get-NextPierdeskVersion '1.0.65535' | Out-Null } catch { $rejected = $true }
Assert $rejected 'Windows version overflow must fail'
$files = @('clipferry-remote-apply.ps1','update-local-clipferry.ps1','local-update-support.ps1','install-update-shortcut.ps1','deploy-clipferry-ssh.ps1','package-clipferry.ps1')
foreach ($file in $files) {
    $errors = $null; $tokens = $null
    $ast = [Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot $file),[ref]$tokens,[ref]$errors)
    Assert ($errors.Count -eq 0) "$file syntax: $errors"
    if ($file -eq 'clipferry-remote-apply.ps1') {
        foreach ($fn in $ast.FindAll({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst]},$false)) {
            . ([scriptblock]::Create($fn.Extent.Text))
        }
    }
}
$receipt = [pscustomobject]@{processId=123;executablePath='C:\Apps\pierdesk.exe';sha256='ABC';version='1.0.1';elevated=$true}
Assert (Test-TrayReceipt $receipt 123 'C:\Apps\pierdesk.exe' 'ABC' '1.0.1.0' $true) 'Valid actual image identity'
Assert (-not (Test-TrayReceipt $receipt 124 'C:\Apps\pierdesk.exe' 'ABC' '1.0.1.0' $true)) 'Wrong PID'
Assert (-not (Test-TrayReceipt $receipt 123 'C:\Other\pierdesk.exe' 'ABC' '1.0.1.0' $true)) 'Wrong image path'
Assert (-not (Test-TrayReceipt $receipt 123 'C:\Apps\pierdesk.exe' 'DEF' '1.0.1.0' $true)) 'Old image hash'
Assert (-not (Test-TrayReceipt $receipt 123 'C:\Apps\pierdesk.exe' 'ABC' '1.0.2.0' $true)) 'Old runtime version'
$receipt.elevated = $false
Assert (-not (Test-TrayReceipt $receipt 123 'C:\Apps\pierdesk.exe' 'ABC' '1.0.1.0' $true)) 'Wrong elevation'
function Get-InteractivePierdeskProcesses { $script:fakeProcesses }
$TargetExe = 'C:\Apps\pierdesk.exe'
$fakeProcesses = @([pscustomobject]@{ExecutablePath=$TargetExe})
Assert ((Find-InstalledPierdeskPath) -eq $TargetExe) 'Exact installed target'
$fakeProcesses += [pscustomobject]@{ExecutablePath='C:\Other\pierdesk.exe'}
$rejected = $false; try { Find-InstalledPierdeskPath | Out-Null } catch { $rejected = $true }
Assert $rejected 'Ambiguous running paths must not select first process'
$fakeProcesses = @([pscustomobject]@{ExecutablePath='C:\Other\pierdesk.exe'})
$rejected = $false; try { Find-InstalledPierdeskPath | Out-Null } catch { $rejected = $true }
Assert $rejected 'Explicit install path must not follow dist copy'
# Process start must verify the returned PID, not another matching process.
function Start-Process { [pscustomobject]@{Id=4321;Handle=1;HasExited=$true} }
function Start-Sleep {}
$rejected = $false; try { Start-InteractivePierdesk 'C:\Apps\pierdesk.exe' | Out-Null } catch { $rejected = $_.Exception.Message -like '*exact updated process exited*' }
Assert $rejected 'Early-exit singleton loser must not be reported successful'
. "$PSScriptRoot\local-update-support.ps1"
$fixture = Join-Path ([IO.Path]::GetTempPath()) ('pierdesk-local-recovery-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $fixture | Out-Null
$staged = Join-Path $fixture 'incoming.exe'
$installed = Join-Path $fixture 'pierdesk.exe'
[IO.File]::WriteAllText($staged, 'isolated fixture, not an executable')
$hash = (Get-FileHash -LiteralPath $staged).Hash
Assert (Initialize-LocalPierdeskTarget $installed $staged $hash) 'Missing local target restored'
Assert ((Get-FileHash -LiteralPath $installed).Hash -eq $hash) 'Recovered target hash exact'
[IO.File]::WriteAllText($staged, 'different incoming fixture')
Assert (-not (Initialize-LocalPierdeskTarget $installed $staged $hash)) 'Existing target preserved'
Assert ((Get-FileHash -LiteralPath $installed).Hash -eq $hash) 'Existing target bytes unchanged'
$badDir = Join-Path $fixture 'bad';New-Item -ItemType Directory -Path $badDir | Out-Null
$badTarget = Join-Path $badDir 'pierdesk.exe'
$rejected=$false;try {Initialize-LocalPierdeskTarget $badTarget $staged $hash | Out-Null} catch {$rejected=$true}
Assert $rejected 'Bad hash rejected'
Assert (-not (Test-Path -LiteralPath $badTarget)) 'Bad hash never published'
Assert (@(Get-ChildItem -LiteralPath $badDir -Force).Count -eq 0) 'Only own temporary copy cleaned'
Write-Host "PASS $count updater/version checks; no desktop input or installed files modified."
