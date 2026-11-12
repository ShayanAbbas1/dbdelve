@echo off
rem Windows blocks .ps1 files by default; Bypass applies to this one run only.
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0Start-Test.ps1" %*
if errorlevel 1 pause
