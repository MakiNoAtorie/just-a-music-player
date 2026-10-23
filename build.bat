@echo off
rem Builds target\release\maki-player.exe. scripts\cargo.ps1 uses the local .tools toolchain when present.
cd /d "%~dp0"
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\cargo.ps1 build --release --locked
if errorlevel 1 (
    echo Build failed.
    pause
    exit /b 1
)
echo Built %~dp0target\release\maki-player.exe
pause
