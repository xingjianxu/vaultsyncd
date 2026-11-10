@echo off
setlocal
"%~dp0vaultsyncd-agent.exe" --user-session start
if errorlevel 1 (
  echo Start failed. State and user content were preserved. See the error above and local agent.log.
  pause
  exit /b 1
)
exit /b 0
