@echo off
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
set PATH=C:\Users\Plas-IT-Radg\.rust-manual\rustc\bin;C:\Users\Plas-IT-Radg\.rust-manual\cargo\bin;%PATH%
set ORT_CACHE_DIR=%USERPROFILE%\.ort-cache
cargo %*
