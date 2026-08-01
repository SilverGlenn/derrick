@echo off
setlocal
REM ============================================================
REM  Derrick updater: pulls the latest source, rebuilds, and
REM  reinstalls to %LOCALAPPDATA%\Derrick.
REM ============================================================
cd /d "%~dp0"

echo [1/3] Pulling latest source...
git pull
if errorlevel 1 (
    echo Pull failed. Aborting.
    exit /b 1
)

echo [2/3] Rebuilding...
call build.cmd build --release
if errorlevel 1 (
    echo Build failed. Aborting.
    exit /b 1
)

set "INSTALL_DIR=%LOCALAPPDATA%\Derrick"
echo [3/3] Installing to %INSTALL_DIR%...
if not exist "%INSTALL_DIR%\assets" mkdir "%INSTALL_DIR%\assets"
copy /y "target\release\derrick.exe" "%INSTALL_DIR%\" >nul
copy /y "target\release\DirectML.dll" "%INSTALL_DIR%\" >nul
copy /y "assets\face_detection_yunet_2026may.onnx" "%INSTALL_DIR%\assets\" >nul

echo.
echo Update installed. Restart Derrick (or close it from the tray and
echo launch it again) to run the new version.
exit /b 0
