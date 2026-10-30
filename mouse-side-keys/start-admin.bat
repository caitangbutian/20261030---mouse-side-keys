@echo off
setlocal enabledelayedexpansion
rem Run the switcher ELEVATED via UAC.
rem
rem WHY: if you normally run cmd / Terminal / your IDE as Administrator,
rem the foreground window is at High integrity while this script runs at
rem Medium. Windows (UIPI) then silently rejects SendInput -- you get
rem "0/6 events sent, err=0" and the side buttons do nothing.
rem Running elevated puts us at the same level, so SendInput is accepted.
rem
rem NOTE: .bat files must stay pure ASCII -- cmd.exe always parses them
rem with the system ANSI codepage (GBK), so UTF-8 Chinese comments break
rem parsing entirely ("xxx is not recognized as a command" errors).

rem Extra arguments for mouse_side_keys.py. Empty = use the script's
rem built-in default (swapped direction: back button -> desktop on the
rem RIGHT, forward button -> LEFT). For the standard direction put
rem --no-swap here. Running mouse_side_keys.py directly behaves the
rem same; this bat only adds elevation for admin foreground windows.
set "ARGS="

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

echo ============================================================
echo  Starting ELEVATED (administrator)
echo ============================================================
echo  A UAC prompt will appear - please click Yes.
echo  The script then moves itself to the background automatically,
echo  so closing windows afterwards will NOT stop it.
echo.
echo  Press any key to request elevation...
pause >nul

if defined ARGS (
    if defined PRE (
        powershell -NoProfile -Command "Start-Process -FilePath '%EXE%' -ArgumentList @('%PRE%','%SCRIPT%','!ARGS!') -Verb RunAs -WorkingDirectory '%DIR%'"
    ) else (
        powershell -NoProfile -Command "Start-Process -FilePath '%EXE%' -ArgumentList @('%SCRIPT%','!ARGS!') -Verb RunAs -WorkingDirectory '%DIR%'"
    )
) else (
    if defined PRE (
        powershell -NoProfile -Command "Start-Process -FilePath '%EXE%' -ArgumentList @('%PRE%','%SCRIPT%') -Verb RunAs -WorkingDirectory '%DIR%'"
    ) else (
        powershell -NoProfile -Command "Start-Process -FilePath '%EXE%' -ArgumentList @('%SCRIPT%') -Verb RunAs -WorkingDirectory '%DIR%'"
    )
)

echo.
echo If the UAC prompt was cancelled, nothing started - run this again.
echo.
pause
exit /b 0
