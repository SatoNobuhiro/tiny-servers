@echo off
echo ============================================
echo  Tiny Servers Diagnostic Tool
echo ============================================
echo.

echo [OS Info]
systeminfo 2>nul | findstr /B /C:"OS Name" /C:"OS Version" /C:"System Type" /C:"OS "
echo.

echo [EXE Info]
set "EXE=%~dp0tiny_servers.exe"
if not exist "%EXE%" set "EXE=%~dp0target\release\tiny_servers.exe"
if exist "%EXE%" (
    echo   Found: %EXE%
    for %%F in ("%EXE%") do echo   Size: %%~zF bytes
) else (
    echo   NOT FOUND
    echo   Place this .bat next to tiny_servers.exe or in the project root.
    pause
    exit /b
)
echo.

echo [GPU / OpenGL]
powershell -NoProfile -Command "Get-CimInstance Win32_VideoController | Select-Object Name,DriverVersion,Status | Format-List" 2>nul
echo.

echo [Font Check]
if exist "%SystemRoot%\Fonts\YuGothR.ttc" (
    echo   [OK]  YuGothR.ttc
) else (
    echo   [MISS] YuGothR.ttc
)
echo.

echo [Direct Launch Test]
echo   Launching tiny_servers.exe ...
echo.
set "LOGFILE=%~dp0diagnose_output.txt"
"%EXE%" >"%LOGFILE%" 2>&1
set "ECODE=%errorlevel%"
echo   Exit code: %ECODE%
echo.

if exist "%LOGFILE%" (
    for %%F in ("%LOGFILE%") do (
        if %%~zF GTR 0 (
            echo   [Captured output:]
            type "%LOGFILE%"
        ) else (
            echo   No console output captured.
        )
    )
    del "%LOGFILE%" >nul 2>&1
)
echo.

echo [Recent Event Log Errors]
powershell -NoProfile -Command "Get-WinEvent -FilterHashtable @{LogName='Application'; Level=2; StartTime=(Get-Date).AddMinutes(-2)} -MaxEvents 5 2>$null | ForEach-Object { Write-Host ('  ' + $_.TimeCreated.ToString('HH:mm:ss') + ' ' + $_.ProviderName + ': ' + $_.Message.Split([char]10)[0]) }"
echo.

pause
