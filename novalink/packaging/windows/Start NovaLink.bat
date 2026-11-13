@echo off
rem Double-click to run the NovaLink agent in this window. Close the window
rem (or press Ctrl-C) to stop it; held keys are released on the way out.
cd /d "%~dp0"
echo NovaLink agent starting. Leave this window open while you use NovaLink.
echo To pair or check status, double-click "NovaLink Terminal.bat".
echo.
novalink-agent.exe %*
echo.
echo NovaLink agent stopped (exit code %ERRORLEVEL%). Logs: %LOCALAPPDATA%\NovaLink\logs\
pause
