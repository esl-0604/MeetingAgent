@echo off
setlocal
cd /d "%~dp0"

echo === MeetingAgent PyInstaller build ===
echo.

if not exist ".venv\Scripts\python.exe" (
    echo ERROR: .venv not found. Create it and install requirements.txt first.
    pause
    exit /b 1
)

rem Clean previous
if exist build rmdir /S /Q build
if exist dist rmdir /S /Q dist
if exist MeetingAgent.spec del /F /Q MeetingAgent.spec

rem Bundle EasyOCR cached models if present
set "EASYOCR_MODEL_SRC=%USERPROFILE%\.EasyOCR\model"
if not exist "%EASYOCR_MODEL_SRC%" (
    echo WARNING: %EASYOCR_MODEL_SRC% not found - building without model bundle.
    set "ADD_DATA_FLAG="
) else (
    echo Bundling EasyOCR models: %EASYOCR_MODEL_SRC%
    set "ADD_DATA_FLAG=--add-data %EASYOCR_MODEL_SRC%;easyocr_models"
)

.venv\Scripts\python.exe -m PyInstaller ^
    --noconfirm ^
    --clean ^
    --onedir ^
    --windowed ^
    --name MeetingAgent ^
    --collect-all easyocr ^
    --collect-all soundcard ^
    --collect-data imagehash ^
    --hidden-import win32timezone ^
    --hidden-import PIL._imagingtk ^
    --hidden-import PIL.ImageTk ^
    %ADD_DATA_FLAG% ^
    meeting_agent_app.py

if %errorlevel% neq 0 (
    echo.
    echo === Build FAILED ===
    pause
    exit /b %errorlevel%
)

rem Copy installer helper files into dist/MeetingAgent/
set "DIST_DIR=%CD%\dist\MeetingAgent"
echo Copying installer helpers from installer\...
xcopy /Y /Q "installer\*.bat" "%DIST_DIR%\" >nul
xcopy /Y /Q "installer\README.txt" "%DIST_DIR%\" >nul

echo.
echo === Build complete ===
echo Exe: %DIST_DIR%\MeetingAgent.exe
echo.
echo Creating release zip...
if exist "dist\MeetingAgent_installer.zip" del /F /Q "dist\MeetingAgent_installer.zip"
powershell -NoProfile -Command "Compress-Archive -Path 'dist\MeetingAgent\*' -DestinationPath 'dist\MeetingAgent_installer.zip' -Force"
echo Release zip: %CD%\dist\MeetingAgent_installer.zip
echo.
pause
