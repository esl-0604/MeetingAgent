@echo off
rem Double-click launcher. Keeps the console window open so logs are visible.
title Meeting Agent
echo ============================================================
echo  Meeting Agent - Teams meeting capture
echo  Press Ctrl-C to stop and finalise the current session.
echo ============================================================
echo.
"%~dp0meeting-agent.exe" %*
echo.
echo Agent exited. Press any key to close this window.
pause >nul
