@echo off
setlocal
"%~dp0vaultsyncd-zip.exe" start
if errorlevel 1 (
  echo Start failed. State and user content were preserved. See the error above and local agent.log.
  pause
  exit /b 1
)
exit /b 0
