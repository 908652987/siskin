@echo off
setlocal

powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0spike\scripts\build-windows-ui.ps1"
set "EXITCODE=%ERRORLEVEL%"

echo.
if not "%EXITCODE%"=="0" (
    echo Build FAILED.
) else (
    echo Build OK: %~dp0target\release\siskin-windows-spike.exe
    echo Double-click that exe to start it.
)

if /I not "%~1"=="--no-pause" pause
exit /b %EXITCODE%
