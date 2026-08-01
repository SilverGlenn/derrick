@echo off
setlocal
REM ============================================================
REM  Derrick installer: builds the release and installs it to
REM  %LOCALAPPDATA%\Derrick (per-user, no admin needed).
REM  The in-app "Start with Windows" toggle will then point at
REM  the installed copy.
REM ============================================================
cd /d "%~dp0"

echo [1/3] Building release...
call build.cmd build --release
if errorlevel 1 (
    echo Build failed. Aborting.
    exit /b 1
)

set "INSTALL_DIR=%LOCALAPPDATA%\Derrick"
echo [2/3] Installing to %INSTALL_DIR%...
if not exist "%INSTALL_DIR%\assets" mkdir "%INSTALL_DIR%\assets"
copy /y "target\release\derrick.exe" "%INSTALL_DIR%\" >nul
copy /y "target\release\DirectML.dll" "%INSTALL_DIR%\" >nul
copy /y "assets\face_detection_yunet_2026may.onnx" "%INSTALL_DIR%\assets\" >nul

echo [3/3] Creating Start Menu shortcut...
powershell -NoProfile -Command ^
  "$ws = New-Object -ComObject WScript.Shell; $s = $ws.CreateShortcut((Join-Path $ws.SpecialFolders('Programs') 'Derrick.lnk')); $s.TargetPath = '%INSTALL_DIR%\derrick.exe'; $s.WorkingDirectory = '%INSTALL_DIR%'; $s.Save()"

echo.
echo Done. Derrick is installed at %INSTALL_DIR%
echo Run it from the Start Menu, or enable "Start with Windows" in its Settings.
exit /b 0
