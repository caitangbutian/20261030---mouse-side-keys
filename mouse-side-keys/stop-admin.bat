@echo off
chcp 65001 >nul
setlocal enabledelayedexpansion
rem Stop the running instance. A normal-privilege taskkill cannot kill an
rem administrator process, so stopping also has to go through UAC.

set "DIR=%~dp0"
set "SCRIPT=%~dp0mouse_side_keys.py"
if not exist "%SCRIPT%" (
    echo [X] mouse_side_keys.py not found next to this script.
    pause
    exit /b 1
)

set "EXE="
set "PRE="
where py.exe >nul 2>nul
if !errorlevel!==0 (
    set "EXE=py.exe"
    set "PRE=-3"
)
if not defined EXE (
    where python.exe >nul 2>nul
    if !errorlevel!==0 (
        set "EXE=python.exe"
        set "PRE="
    )
)
if not defined EXE (
    echo [X] Python was not found on PATH.
    pause
    exit /b 1
)

echo Requesting elevation to stop the running instance (UAC prompt)...
if defined PRE (
    powershell -NoProfile -Command "Start-Process -FilePath '%EXE%' -ArgumentList @('%PRE%','%SCRIPT%','--stop') -Verb RunAs -WorkingDirectory '%DIR%' -Wait"
) else (
    powershell -NoProfile -Command "Start-Process -FilePath '%EXE%' -ArgumentList @('%SCRIPT%','--stop') -Verb RunAs -WorkingDirectory '%DIR%' -Wait"
)
echo.
pause
exit /b 0
