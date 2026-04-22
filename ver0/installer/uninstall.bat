@echo off
chcp 65001 >nul
setlocal

echo =====================================
echo    Meeting Agent Uninstaller
echo =====================================
echo.

set "INSTALL_DIR=%LOCALAPPDATA%\MeetingAgent"

echo [1/4] Stopping any running instance...
taskkill /F /IM MeetingAgent.exe >nul 2>&1

echo [2/4] Removing URL protocol (meetingagent://)...
reg delete "HKCU\Software\Classes\meetingagent" /f >nul 2>&1

echo [3/4] Removing shortcuts...
del "%USERPROFILE%\Desktop\Meeting Agent.lnk" >nul 2>&1
del "%APPDATA%\Microsoft\Windows\Start Menu\Programs\Meeting Agent.lnk" >nul 2>&1

echo [4/4] Scheduling install folder deletion (3s)...
echo.
echo Will remove: %INSTALL_DIR%
echo Your saved recordings in other folders are untouched.
echo.
pause

start "" /b cmd /c "timeout /t 3 >nul & rmdir /S /Q "%INSTALL_DIR%""
