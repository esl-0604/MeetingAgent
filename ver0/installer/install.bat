@echo off
chcp 65001 >nul
setlocal EnableDelayedExpansion

echo =====================================
echo    Meeting Agent Installer
echo =====================================
echo.

set "INSTALL_DIR=%LOCALAPPDATA%\MeetingAgent"
set "SRC_DIR=%~dp0"
if "%SRC_DIR:~-1%"=="\" set "SRC_DIR=!SRC_DIR:~0,-1!"

echo Install path: %INSTALL_DIR%
echo Source path : %SRC_DIR%
echo.

taskkill /F /IM MeetingAgent.exe >nul 2>&1

if not exist "%INSTALL_DIR%" mkdir "%INSTALL_DIR%"

echo [1/4] Copying files...
xcopy /E /Y /I /Q "%SRC_DIR%\*" "%INSTALL_DIR%\" >nul
if errorlevel 1 (
    echo Copy failed. Check disk space or permissions.
    pause
    exit /b 1
)

set "EXE_PATH=%INSTALL_DIR%\MeetingAgent.exe"

echo [2/4] Registering URL protocol (meetingagent://)...
reg add "HKCU\Software\Classes\meetingagent" /ve /d "URL:Meeting Agent Protocol" /f >nul
reg add "HKCU\Software\Classes\meetingagent" /v "URL Protocol" /d "" /f >nul
reg add "HKCU\Software\Classes\meetingagent\DefaultIcon" /ve /d "\"%EXE_PATH%\",0" /f >nul
reg add "HKCU\Software\Classes\meetingagent\shell\open\command" /ve /d "\"%EXE_PATH%\" \"%%1\"" /f >nul

echo [3/4] Creating shortcuts (Desktop + Start Menu)...
set "DesktopLnk=%USERPROFILE%\Desktop\Meeting Agent.lnk"
set "StartMenuLnk=%APPDATA%\Microsoft\Windows\Start Menu\Programs\Meeting Agent.lnk"
powershell -NoProfile -ExecutionPolicy Bypass -Command "$s=(New-Object -COM WScript.Shell).CreateShortcut('%DesktopLnk%'); $s.TargetPath='%EXE_PATH%'; $s.WorkingDirectory='%INSTALL_DIR%'; $s.Description='Meeting Agent'; $s.Save()"
powershell -NoProfile -ExecutionPolicy Bypass -Command "$s=(New-Object -COM WScript.Shell).CreateShortcut('%StartMenuLnk%'); $s.TargetPath='%EXE_PATH%'; $s.WorkingDirectory='%INSTALL_DIR%'; $s.Description='Meeting Agent'; $s.Save()"

echo [4/4] Writing install info...
(
    echo INSTALL_DIR=%INSTALL_DIR%
    echo INSTALLED_AT=%DATE% %TIME%
    echo PROTOCOL=meetingagent
) > "%INSTALL_DIR%\.install_info.txt"

echo.
echo =====================================
echo    Install complete
echo =====================================
echo.
echo Now Slack meetingagent:// links will launch this app.
echo Desktop / Start Menu shortcuts were created.
echo.
echo Install location: %INSTALL_DIR%
echo To uninstall: run %INSTALL_DIR%\uninstall.bat
echo.
pause
