@echo off
setlocal
REM ============================================================
REM  Builds the MSI installer: Derrick-<version>.msi in dist\.
REM  Requires tools\wix\ (downloads it automatically) and a
REM  successful release build.
REM ============================================================
cd /d "%~dp0"

echo [1/3] Building release...
call build.cmd build --release
if errorlevel 1 (
    echo Build failed. Aborting.
    exit /b 1
)

if not exist "tools\wix\candle.exe" (
    echo [1b/3] Downloading WiX toolset...
    if not exist "tools" mkdir tools
    curl -sL -o tools\wix314-binaries.zip https://github.com/wixtoolset/wix3/releases/download/wix3141rtm/wix314-binaries.zip
    powershell -NoProfile -Command "Expand-Archive -Path 'tools\wix314-binaries.zip' -DestinationPath 'tools\wix' -Force"
    del tools\wix314-binaries.zip
)

for /f "tokens=3" %%v in ('findstr /b "version" Cargo.toml') do set VERSION=%%v
set VERSION=%VERSION:"=%
echo [2/3] Packaging Derrick %VERSION%...
if not exist "dist" mkdir dist
if not exist "staging" mkdir staging

tools\wix\candle.exe -ext WixUIExtension -out staging\Derrick.wixobj Derrick.wxs
if errorlevel 1 (
    echo WiX compile failed. Aborting.
    exit /b 1
)
tools\wix\light.exe -ext WixUIExtension -sice:ICE91 -out "dist\Derrick-%VERSION%.msi" staging\Derrick.wixobj
if errorlevel 1 (
    echo WiX link failed. Aborting.
    exit /b 1
)

echo [3/3] Done: dist\Derrick-%VERSION%.msi
exit /b 0
