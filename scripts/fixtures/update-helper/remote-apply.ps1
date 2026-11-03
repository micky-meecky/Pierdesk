param([string]$StagedExe,[string]$ExpectedSha256,[string]$StatusPath,[string]$ExpectedHostname,[string]$TargetExe,[switch]$Force)
# Isolated fixture only: does not exit, replace or start any installed tray.
Add-Type 'using System; using System.Runtime.InteropServices; public static class UpdateConsoleFixture { [DllImport("kernel32.dll")] public static extern IntPtr GetConsoleWindow(); }'
[ordered]@{consoleWindow=[UpdateConsoleFixture]::GetConsoleWindow().ToInt64();target=$TargetExe;force=$Force.IsPresent} |
    ConvertTo-Json | Set-Content -LiteralPath $StatusPath -Encoding UTF8
if ($ExpectedHostname -eq 'simulate-failure') { exit 7 }
exit 0
