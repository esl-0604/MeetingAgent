@echo off
setlocal
cd /d "%~dp0"

echo === MeetingAgent PyInstaller 빌드 시작 ===
echo.

if not exist ".venv\Scripts\python.exe" (
    echo ERROR: .venv 가 없습니다. 먼저 venv 생성 후 requirements.txt 설치하세요.
    pause
    exit /b 1
)

rem 이전 빌드 정리
if exist build rmdir /S /Q build
if exist dist rmdir /S /Q dist
if exist MeetingAgent.spec del /F /Q MeetingAgent.spec

rem EasyOCR 모델 경로 확인 (사용자 홈에 캐시된 모델을 번들)
set "EASYOCR_MODEL_SRC=%USERPROFILE%\.EasyOCR\model"
if not exist "%EASYOCR_MODEL_SRC%" (
    echo WARNING: %EASYOCR_MODEL_SRC% 없음 - 모델 번들 없이 빌드합니다.
    echo          첫 OCR 실행 시 인터넷에서 모델 다운로드 필요.
    set "ADD_DATA_FLAG="
) else (
    echo EasyOCR 모델 번들: %EASYOCR_MODEL_SRC%
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
    %ADD_DATA_FLAG% ^
    meeting_agent_app.py

if %errorlevel% neq 0 (
    echo.
    echo === 빌드 실패 ===
    pause
    exit /b %errorlevel%
)

rem 배포용 폴더 안에 런처 바로가기(+README) 생성
set "DIST_DIR=%CD%\dist\MeetingAgent"
echo @echo off > "%DIST_DIR%\실행.bat"
echo cd /d "%%~dp0" >> "%DIST_DIR%\실행.bat"
echo start "" "MeetingAgent.exe" >> "%DIST_DIR%\실행.bat"

echo === MeetingAgent 사용법 === > "%DIST_DIR%\README.txt"
echo. >> "%DIST_DIR%\README.txt"
echo 1. 이 폴더를 원하는 위치에 통째로 둡니다 (폴더 이동 OK). >> "%DIST_DIR%\README.txt"
echo 2. MeetingAgent.exe 를 더블클릭하거나 실행.bat 를 씁니다. >> "%DIST_DIR%\README.txt"
echo 3. 첫 실행 시 '자막 영역 선택' 버튼으로 Teams 자막 영역을 드래그 지정하세요. >> "%DIST_DIR%\README.txt"
echo 4. Teams 미팅을 시작하면 자동으로 감지합니다. >> "%DIST_DIR%\README.txt"
echo 5. 미팅 종료 시 저장할 폴더를 선택하면 audio / transcript / slides 가 저장됩니다. >> "%DIST_DIR%\README.txt"
echo. >> "%DIST_DIR%\README.txt"
echo 설정 파일: 이 폴더 안 meeting_agent_config.json >> "%DIST_DIR%\README.txt"

echo.
echo === 빌드 완료 ===
echo 실행 파일: %DIST_DIR%\MeetingAgent.exe
echo.
echo 배포 zip 압축 중...
powershell -Command "Compress-Archive -Path 'dist\MeetingAgent\*' -DestinationPath 'dist\MeetingAgent_portable.zip' -Force"
echo 배포 zip : %CD%\dist\MeetingAgent_portable.zip
echo.
pause
