@echo off
setlocal
echo This pauses all bindings, exports unconfirmed content, drains callbacks and stops the agent.
echo Bindings remain PAUSED after restart. Resume desired bindings explicitly in the GUI.
echo Root, cache, dirty content and cloud provider registration are NOT deleted.
choice /C YN /N /M "Stop synchronization safely? [Y/N] "
if errorlevel 2 exit /b 0
if errorlevel 1 goto stop
exit /b 1
:stop
"%~dp0vaultsyncd-agent.exe" --user-session stop
if errorlevel 1 (
  echo Stop failed. No force kill was attempted. Bindings may already be paused. Inspect agent.log.
  pause
  exit /b 1
)
pause
exit /b 0
