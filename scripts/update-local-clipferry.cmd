@echo off
setlocal
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0update-local-clipferry.ps1"
if errorlevel 1 (
  echo.
  echo Pierdesk local update failed. Review the message above.
  pause
  exit /b 1
)
exit /b 0
