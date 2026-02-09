@echo off
setlocal
set LOG=C:\OEM\windows_gui_smoke_install.log
echo [info] starting windows gui smoke hook > "%LOG%"
powershell -NoProfile -ExecutionPolicy Bypass -File "C:\OEM\windows_gui_smoke.ps1" >> "%LOG%" 2>&1
if errorlevel 1 (
  echo [error] windows gui smoke hook failed >> "%LOG%"
) else (
  echo [info] windows gui smoke hook passed >> "%LOG%"
)
endlocal
