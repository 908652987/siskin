@echo off
setlocal

echo Checking Windows build prerequisites...
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0spike\scripts\check-windows-ui-prereqs.ps1"
set "CHECK=%ERRORLEVEL%"

if not "%CHECK%"=="0" (
    echo.
    echo Install Rust with the MSVC toolchain and Visual Studio Build Tools first.
    if /I not "%~1"=="--no-pause" pause
    exit /b %CHECK%
)

echo.
echo Preparing the local development signing certificate...
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0spike\scripts\setup-dev-signing.ps1"
set "SETUP=%ERRORLEVEL%"

echo.
if not "%SETUP%"=="0" (
    echo Setup FAILED.
) else (
    echo Setup OK. Run build-windows.cmd to build a signed exe.
)

if /I not "%~1"=="--no-pause" pause
exit /b %SETUP%
