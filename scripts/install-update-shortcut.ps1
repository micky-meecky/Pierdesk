# Adds a GUI-script-host shortcut. The legacy .cmd remains a diagnostic fallback.
$ErrorActionPreference = 'Stop'
$desktop = [Environment]::GetFolderPath('Desktop')
$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut((Join-Path $desktop '更新 Pierdesk（静默）.lnk'))
$shortcut.TargetPath = "$env:SystemRoot\System32\wscript.exe"
$shortcut.Arguments = '//nologo "' + (Join-Path $PSScriptRoot 'update-local-clipferry.vbs') + '"'
$shortcut.WorkingDirectory = $PSScriptRoot
$shortcut.IconLocation = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\dist\pierdesk.exe')) + ',0'
$shortcut.Description = '静默更新并重启 Pierdesk；需要管理员权限时保留 Windows UAC 确认。'
$shortcut.Save()
Write-Host $shortcut.FullName
$startPath = Join-Path $desktop '启动 Pierdesk.lnk'
$start = $shell.CreateShortcut($startPath)
$start.TargetPath = Join-Path $desktop 'pierdesk.exe'
$start.Arguments = 'tray'
$start.WorkingDirectory = $desktop
$start.IconLocation = $shortcut.IconLocation
$start.Description = '启动已安装的 Pierdesk（不更新）；需要时确认管理员 UAC。'
$start.Save()
# Shell Link LinkFlags SLDF_RUNAS_USER: preserve the existing shortcut fields.
$bytes = [IO.File]::ReadAllBytes($startPath)
$bytes[0x15] = $bytes[0x15] -bor 0x20
[IO.File]::WriteAllBytes($startPath, $bytes)
Write-Host $startPath
