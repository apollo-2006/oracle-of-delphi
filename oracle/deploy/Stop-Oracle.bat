@echo off
rem Oracle of Delphi — stop everything.
rem Core reaps its children when it exits cleanly, but a hard stop won't, so kill
rem the whole set. Adjust the LLM line if your server exe is named differently.
taskkill /IM oracle-core.exe /F >nul 2>&1
taskkill /IM oracle-actd.exe /F >nul 2>&1
taskkill /IM llama-server.exe /F >nul 2>&1
echo The Oracle has been dismissed.
