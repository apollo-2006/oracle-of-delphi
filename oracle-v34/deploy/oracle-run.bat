@echo off
rem Oracle of Delphi — the actual startup command (run hidden by Oracle.vbs).
rem Starts oracle-core, which supervises the LLM server + actd and opens the HUD.
rem Core's own log is captured here; the children log under %APPDATA%\oracle\run.
setlocal

set "CFG=%APPDATA%\oracle\oracle.toml"
set "RUNDIR=%APPDATA%\oracle\run"
if not exist "%RUNDIR%" mkdir "%RUNDIR%" 2>nul

rem Prefer the built binary in the repo; fall back to one sitting next to this script.
set "EXE=%~dp0..\target\release\oracle-core.exe"
if not exist "%EXE%" set "EXE=%~dp0oracle-core.exe"

if not exist "%EXE%" (
    echo Could not find oracle-core.exe near "%~dp0". Build it first: cargo build --release -p oracle-core >> "%RUNDIR%\oracle.log" 2>&1
    exit /b 1
)

"%EXE%" run --config "%CFG%" >> "%RUNDIR%\oracle.log" 2>&1
