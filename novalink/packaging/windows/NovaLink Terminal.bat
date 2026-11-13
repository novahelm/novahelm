@echo off
rem Opens a window where the `novalink` command works (pair, status, layout, metrics...).
cd /d "%~dp0"
set "PATH=%~dp0;%PATH%"
echo NovaLink commands are ready. Try:
echo   novalink status
echo   novalink discovered
echo   novalink pair ^<name^>        then  novalink pair pending  /  novalink pair accept ^<id^>
echo   novalink layout right ^<name^>
echo   novalink doctor
echo.
cmd /k
